//! The live draft. The open editor holds the only editable copy.
//!
//! Agent edits and `draft.md` writes share this merge. Neither path transmits.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::AccountId;

use super::frontmatter::{
    DraftFields, DraftIssue, email_of, parse_address_line, parse_draft, write_draft,
};
use super::merge::{self, HunkKind};
use super::mime::{self, Outgoing, ResolvedFile};
use super::persist;
use super::workspace::{self, Closure};

const DEBOUNCE_MS: u64 = 150;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Hunk {
    id: u64,
    kind: HunkKind,
    previous: String,
    inserted: String,
    /// Byte start of `inserted` in the serialized draft, when it was applied.
    at: Option<usize>,
}

#[derive(Debug, Clone)]
struct Checkpoint {
    fields: DraftFields,
    body: String,
    hunks: Vec<Hunk>,
    notices: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddressView {
    pub raw: String,
    pub email: String,
    pub fresh: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HunkView {
    pub id: u64,
    pub kind: HunkKind,
    pub start: usize,
    pub end: usize,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentView {
    pub path: String,
    pub name: String,
    pub bytes: u64,
    pub inline: bool,
    pub missing: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub id: String,
    pub revision: u64,
    pub from: String,
    pub to: Vec<AddressView>,
    pub cc: Vec<AddressView>,
    pub bcc: Vec<AddressView>,
    pub subject: String,
    pub body: String,
    pub in_reply_to: Option<String>,
    pub hunks: Vec<HunkView>,
    pub issues: Vec<DraftIssue>,
    pub notices: Vec<String>,
    pub attachments: Vec<AttachmentView>,
    pub unreviewed: usize,
    pub quoted: Option<String>,
    pub open: bool,
    pub focused: bool,
    pub send_block: Option<String>,
    pub text: String,
}

pub struct DraftSession {
    id: String,
    folder: PathBuf,
    account_id: AccountId,
    fields: DraftFields,
    body: String,
    revision: u64,
    seen_revision: u64,
    /// Path and content hash of referenced files the person has accepted.
    /// A later text edit does not replace a hash that is already recorded.
    painted_files: Option<Vec<(String, u64)>>,
    /// The next paint may replace accepted attachment hashes.
    accept_files: bool,
    locked_reply: Option<String>,
    hunks: Vec<Hunk>,
    next_hunk: u64,
    issues: Vec<DraftIssue>,
    notices: Vec<String>,
    undo: Vec<Checkpoint>,
    undo_open: bool,
    last_written: String,
    /// Serialized draft before the person's uncommitted edits. Agent edits
    /// that touch a line changed since this snapshot are refused.
    settled: String,
    /// Mailbox database that should receive this draft on flush.
    store_path: Option<PathBuf>,
    /// When the latest unflushed editor change was armed. `None` when the file is current.
    outbound_since: Option<u64>,
    /// Path, length, and mtime of attachments last copied into the mailbox.
    attachment_stamp: Vec<(String, u64, u128)>,
    pending: Option<String>,
    pending_delete: bool,
    pending_since: u64,
    open: bool,
    focused: bool,
    known: HashSet<String>,
    identities: HashSet<String>,
    owner: String,
    agent_seen: HashMap<String, String>,
    quoted: Option<String>,
}

impl DraftSession {
    pub fn create(
        folder: PathBuf,
        id: impl Into<String>,
        account_id: AccountId,
        fields: DraftFields,
        body: impl Into<String>,
    ) -> Result<Self, String> {
        let body = body.into();
        let locked_reply = fields.in_reply_to.clone();
        let mut session = Self {
            id: id.into(),
            folder,
            account_id,
            fields,
            body,
            revision: 1,
            seen_revision: 0,
            painted_files: None,
            accept_files: false,
            locked_reply,
            hunks: Vec::new(),
            next_hunk: 1,
            issues: Vec::new(),
            notices: Vec::new(),
            undo: Vec::new(),
            undo_open: false,
            last_written: String::new(),
            settled: String::new(),
            store_path: None,
            outbound_since: None,
            attachment_stamp: Vec::new(),
            pending: None,
            pending_delete: false,
            pending_since: 0,
            open: false,
            focused: false,
            known: HashSet::new(),
            identities: HashSet::new(),
            owner: String::new(),
            agent_seen: HashMap::new(),
            quoted: None,
        };
        session.locked_reply = session.fields.in_reply_to.clone();
        if let Some(email) = email_of(&session.fields.from) {
            session.identities.insert(email);
        }
        session.load_hunks();
        session.settled = session.serialize();
        session.flush()?;
        Ok(session)
    }

    pub fn open_folder(folder: &Path, account_hint: AccountId) -> Result<Self, String> {
        let id = folder
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("draft")
            .to_string();
        let Some(store) = persist::open_store_for_draft(&id) else {
            return Err("draft has no mailbox record".into());
        };
        Self::open_with_store(folder, account_hint, &store)
    }

    pub fn open_with_store(
        folder: &Path,
        account_hint: AccountId,
        store: &crate::Store,
    ) -> Result<Self, String> {
        let id = folder
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("draft")
            .to_string();
        let Some(stored) = store.load_compose(&id).map_err(|err| err.to_string())? else {
            return Err("draft has no mailbox record".into());
        };
        if account_hint.as_str() != stored.account_id.as_str() {
            return Err("account does not match the mailbox".into());
        }
        let text = workspace::read_draft_file(folder)?;
        let parsed = parse_draft(&text).map_err(|err| err.message)?;
        if parsed.fields.in_reply_to != stored.fields.in_reply_to {
            return Err("reply target does not match the mailbox".into());
        }
        let mut session = Self::create(
            folder.to_path_buf(),
            id,
            stored.account_id,
            parsed.fields,
            parsed.body,
        )?;
        session.identities = stored.identities.into_iter().collect();
        session.owner = stored.owner;
        session.locked_reply = stored.fields.in_reply_to.clone();
        session.fields.in_reply_to = session.locked_reply.clone();
        session.issues = parsed.issues;
        if let Some(path) = store.db_path() {
            session.store_path = Some(path.to_path_buf());
        }
        if let Ok(known) = store.mailed_addresses(session.account_id()) {
            session.known = known;
        }
        Ok(session)
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn folder(&self) -> &Path {
        &self.folder
    }

    pub fn account_id(&self) -> &AccountId {
        &self.account_id
    }

    pub fn owner(&self) -> &str {
        &self.owner
    }

    pub fn set_owner(&mut self, owner: impl Into<String>) {
        self.owner = owner.into();
    }

    /// The mailbox row is the credential. An empty owner matches nobody.
    pub fn allows_agent(&self, agent: &str) -> bool {
        !self.owner.is_empty() && self.owner == agent
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn serialize(&self) -> String {
        write_draft(&self.fields, &self.body)
    }

    pub fn set_known_recipients(&mut self, known: HashSet<String>) {
        self.known = known;
    }

    pub fn set_quote(&mut self, html: Option<String>) {
        self.quoted = html;
    }

    pub fn mark_seen(&mut self) {
        if self.seen_revision == self.revision && !self.accept_files {
            return;
        }
        let current = referenced_file_hashes(&self.folder, &self.fields, &self.body);
        if self.painted_files.is_none() || self.accept_files {
            self.painted_files = Some(current);
            self.accept_files = false;
        } else if let Some(painted) = &mut self.painted_files {
            painted.retain(|(path, _)| current.iter().any(|(name, _)| name == path));
            for (path, hash) in current {
                if painted.iter().all(|(name, _)| name != &path) {
                    painted.push((path, hash));
                }
            }
            painted.sort();
        }
        self.seen_revision = self.revision;
    }

    /// What the compose window repaints for. Seen-ness is not included, so
    /// marking a draft seen does not wake the window again.
    pub fn paint_generation(&self) -> (u64, bool, bool, usize) {
        (self.revision, self.open, self.focused, self.hunks.len())
    }

    pub fn open_for_edit(&mut self) {
        self.open = true;
        self.focused = true;
    }

    /// Focus the editor. Does not transmit.
    pub fn request_send(&mut self) -> bool {
        self.open = true;
        self.focused = true;
        false
    }

    pub fn try_agent_delete(&self) -> Result<(), String> {
        if self.open {
            Err("a draft open in the editor cannot be deleted by an agent".into())
        } else {
            Ok(())
        }
    }

    pub fn begin_user_change(&mut self) {
        if !self.undo_open {
            self.undo.push(self.checkpoint());
            self.undo_open = true;
        }
    }

    pub fn end_user_change(&mut self) {
        self.undo_open = false;
    }

    pub fn set_body_from_user(&mut self, body: String) {
        if body == self.body {
            return;
        }
        self.begin_user_change();
        self.body = body;
        self.bump();
        self.reconcile_hunks();
        self.refresh_issues();
    }

    pub fn set_subject_from_user(&mut self, subject: String) {
        if subject == self.fields.subject {
            return;
        }
        self.begin_user_change();
        self.fields.subject = subject;
        self.bump();
        self.refresh_issues();
    }

    pub fn set_from_user(&mut self, from: String) {
        if from == self.fields.from {
            return;
        }
        self.begin_user_change();
        self.fields.from = from;
        self.bump();
        self.refresh_issues();
    }

    pub fn set_addresses_from_user(&mut self, field: &str, line: &str) -> Result<(), String> {
        let parsed = parse_address_line(line)?;
        self.begin_user_change();
        match field {
            "to" => self.fields.to = parsed,
            "cc" => self.fields.cc = parsed,
            "bcc" => self.fields.bcc = parsed,
            _ => return Err("unknown address field".into()),
        }
        self.bump();
        self.refresh_issues();
        Ok(())
    }

    pub fn undo(&mut self) -> bool {
        self.undo_open = false;
        let Some(previous) = self.undo.pop() else {
            return false;
        };
        self.fields = previous.fields;
        self.body = previous.body;
        self.hunks = previous.hunks;
        self.notices = previous.notices;
        self.bump();
        self.refresh_issues();
        true
    }

    pub fn keep_hunk(&mut self, id: u64) -> bool {
        let Some(index) = self.hunks.iter().position(|hunk| hunk.id == id) else {
            return false;
        };
        let kind = self.hunks[index].kind;
        self.begin_user_change();
        self.end_user_change();
        self.hunks.remove(index);
        if kind == HunkKind::Attachments {
            self.accept_files = true;
        }
        self.bump();
        self.refresh_issues();
        true
    }

    pub fn reject_hunk(&mut self, id: u64) -> bool {
        let Some(index) = self.hunks.iter().position(|hunk| hunk.id == id) else {
            return false;
        };
        let hunk = self.hunks[index].clone();
        let text = self.serialize();
        if hunk.inserted.is_empty() || !text.contains(&hunk.inserted) {
            self.hunks.remove(index);
            return false;
        }
        self.begin_user_change();
        self.end_user_change();
        let next = match revert_span(&text, &hunk) {
            Some(next) => next,
            None => return false,
        };
        if self.adopt_text(&next).is_err() {
            return false;
        }
        self.hunks.retain(|item| item.id != id);
        true
    }

    pub fn edit(&mut self, base_revision: u64, old: &str, new: &str) -> Result<(), String> {
        let current = self.serialize();
        let _ = base_revision;
        let at = merge::find_unique(&current, old)
            .map_err(|err| format!("{err}\nrevision: {}\n{current}", self.revision))?;
        if merge::span_overlaps_user_change(&self.settled, &current, at, at + old.len()) {
            return Err(format!(
                "the text you tried to edit has changed\nrevision: {}\n{current}",
                self.revision
            ));
        }
        let mut next = current.clone();
        next.replace_range(at..at + old.len(), new);
        self.undo.push(self.checkpoint());
        self.adopt_text(&next)?;
        if old != new && self.serialize().contains(new) {
            let at = span_after_edit(&self.serialize(), new, at);
            self.push_hunk(HunkKind::Body, old.to_string(), new.to_string(), at);
        }
        self.settled = self.serialize();
        Ok(())
    }

    pub fn replace_body(&mut self, body: &str) -> Result<(), String> {
        if body == self.body {
            return Ok(());
        }
        self.undo.push(self.checkpoint());
        let previous = self.body.clone();
        self.body = body.to_string();
        self.push_hunk(HunkKind::Body, previous, body.to_string(), None);
        self.bump();
        self.settled = self.serialize();
        Ok(())
    }

    pub fn take_user_changes(&mut self, agent: &str) -> String {
        let current = self.user_serialized();
        let previous = self
            .agent_seen
            .get(agent)
            .cloned()
            .unwrap_or_else(|| current.clone());
        let diff = merge::unified_diff(&previous, &current);
        self.agent_seen.insert(agent.to_string(), current);
        diff
    }

    pub fn push_disk(&mut self, content: Option<String>, now_ms: u64) {
        match content {
            None => {
                self.pending = None;
                self.pending_delete = true;
                self.pending_since = now_ms;
            }
            Some(text) if text == self.last_written => {
                self.pending = None;
                self.pending_delete = false;
            }
            Some(text) => {
                self.pending = Some(text);
                self.pending_delete = false;
                self.pending_since = now_ms;
            }
        }
    }

    pub fn poll_disk(&mut self, now_ms: u64) -> bool {
        if self.pending.is_none() && !self.pending_delete {
            return false;
        }
        if now_ms.saturating_sub(self.pending_since) < DEBOUNCE_MS {
            return false;
        }
        if self.pending_delete {
            self.pending_delete = false;
            self.notices
                .push("draft.md was deleted; restored it from the open draft".into());
            let _ = self.flush();
            return true;
        }
        let Some(text) = self.pending.take() else {
            return false;
        };
        if text == self.last_written {
            return false;
        }
        self.merge_disk(&text);
        true
    }

    /// Remember the mailbox file so a later flush can persist this draft.
    pub fn bind_store(&mut self, path: impl Into<PathBuf>) {
        self.store_path = Some(path.into());
    }

    /// Record an editor change that should hit disk after the outbound debounce.
    pub fn arm_outbound(&mut self, now_ms: u64) {
        self.outbound_since = Some(now_ms);
    }

    /// Write and fsync once the outbound debounce has elapsed.
    pub fn poll_outbound(&mut self, now_ms: u64) -> bool {
        let Some(since) = self.outbound_since else {
            return false;
        };
        if now_ms.saturating_sub(since) < DEBOUNCE_MS {
            return false;
        }
        self.flush().is_ok()
    }

    pub fn flush(&mut self) -> Result<(), String> {
        self.outbound_since = None;
        let text = self.serialize();
        workspace::write_draft_file(&self.folder, &text)?;
        self.last_written = text;
        self.save_hunks();
        self.persist_mailbox();
        Ok(())
    }

    fn persist_mailbox(&mut self) {
        let path = self
            .store_path
            .clone()
            .or_else(|| persist::remembered_store_path(&self.id));
        let Some(path) = path.filter(|path| path.is_file()) else {
            return;
        };
        let Ok(store) = crate::Store::open(&path) else {
            return;
        };
        let draft = persist::StoredCompose {
            id: self.id.clone(),
            account_id: self.account_id.clone(),
            fields: self.fields.clone(),
            body: self.body.clone(),
            files: Vec::new(),
            identities: self.identities.iter().cloned().collect(),
            owner: self.owner.clone(),
        };
        let stamp = attachment_stamp(&self.folder, &self.fields, &self.body);
        if stamp == self.attachment_stamp {
            let _ = store.save_compose_text(&draft);
            return;
        }
        let files = workspace::resolve_closure(&self.folder, &self.fields, &self.body).files;
        let _ = store.save_compose(&persist::StoredCompose { files, ..draft });
        self.attachment_stamp = stamp;
    }

    pub fn closure(&self) -> Closure {
        let mut closure = workspace::resolve_closure(&self.folder, &self.fields, &self.body);
        closure.issues.extend(self.issues.clone());
        closure
    }

    pub fn prepare_send(&self) -> Result<Outgoing, String> {
        if let Some(reason) = self.send_block() {
            return Err(reason);
        }
        let closure = workspace::resolve_closure(&self.folder, &self.fields, &self.body);
        if let Some(issue) = closure.issues.iter().find(|issue| issue.blocks_send) {
            return Err(issue.message.clone());
        }
        Ok(mime::build_outgoing(
            self.account_id.clone(),
            &self.fields,
            &self.body,
            &closure.files,
        ))
    }

    pub fn snapshot(&self) -> Snapshot {
        let text = self.serialize();
        let (listed, listed_issues) =
            workspace::list_closure(&self.folder, &self.fields, &self.body);
        let mut issues = self.issues.clone();
        issues.extend(listed_issues);
        let hunks = self
            .hunks
            .iter()
            .filter_map(|hunk| hunk_view(hunk, &text))
            .collect::<Vec<_>>();
        Snapshot {
            id: self.id.clone(),
            revision: self.revision,
            from: self.fields.from.clone(),
            to: self.address_views(&self.fields.to),
            cc: self.address_views(&self.fields.cc),
            bcc: self.address_views(&self.fields.bcc),
            subject: self.fields.subject.clone(),
            body: self.body.clone(),
            in_reply_to: self.locked_reply.clone(),
            hunks,
            issues,
            notices: self.notices.clone(),
            attachments: listed
                .iter()
                .map(|file| AttachmentView {
                    path: file.path.clone(),
                    name: file.display_name.clone(),
                    bytes: file.len,
                    inline: file.inline,
                    missing: false,
                })
                .collect(),
            unreviewed: self.hunks.len(),
            quoted: self.quoted.clone(),
            open: self.open,
            focused: self.focused,
            send_block: self.send_block(),
            text,
        }
    }

    pub fn fields(&self) -> &DraftFields {
        &self.fields
    }

    pub fn body(&self) -> &str {
        &self.body
    }

    pub fn resolved_files(&self) -> Vec<ResolvedFile> {
        workspace::resolve_closure(&self.folder, &self.fields, &self.body).files
    }

    fn send_block(&self) -> Option<String> {
        if self.seen_revision != self.revision {
            return Some("the draft changed; it is on screen before send".into());
        }
        if let Some(painted) = &self.painted_files {
            let current = referenced_file_hashes(&self.folder, &self.fields, &self.body);
            if current != *painted {
                return Some("an attachment changed after it was shown".into());
            }
        }
        if let Some(issue) = self.issues.iter().find(|issue| issue.blocks_send) {
            return Some(issue.message.clone());
        }
        if self.fields.to.is_empty() {
            return Some("add at least one recipient".into());
        }
        let from_email = email_of(&self.fields.from);
        if from_email
            .as_ref()
            .is_none_or(|email| !self.identities.contains(email))
        {
            return Some("from is not one of your addresses".into());
        }
        let (_files, issues) = workspace::list_closure(&self.folder, &self.fields, &self.body);
        issues
            .iter()
            .find(|issue| issue.blocks_send)
            .map(|issue| issue.message.clone())
    }

    fn merge_disk(&mut self, text: &str) {
        if parse_draft(text).is_err() {
            self.notices.push(
                "draft.md could not be parsed; kept the open draft and rewrote the file".into(),
            );
            let _ = self.flush();
            return;
        }
        let outcome = merge::merge_draft(&self.settled, &self.serialize(), text);
        self.notices.extend(outcome.skipped);
        if self.adopt_text(&outcome.text).is_err() {
            self.notices.push(
                "draft.md could not be parsed; kept the open draft and rewrote the file".into(),
            );
            let _ = self.flush();
            return;
        }
        for hunk in outcome.hunks {
            self.push_hunk(hunk.kind, hunk.previous, hunk.inserted, None);
        }
        self.settled = self.serialize();
        let _ = self.flush();
    }

    fn adopt_text(&mut self, text: &str) -> Result<(), String> {
        let parsed = parse_draft(text).map_err(|err| err.message)?;
        let mut fields = parsed.fields;
        if fields.in_reply_to != self.locked_reply {
            fields.in_reply_to = self.locked_reply.clone();
            self.notices
                .push("in_reply_to is set by Fell; the edit was ignored".into());
        }
        self.fields = fields;
        self.body = parsed.body;
        self.issues = parsed.issues;
        self.bump();
        Ok(())
    }

    fn refresh_issues(&mut self) {
        if let Ok(parsed) = parse_draft(&self.serialize()) {
            self.issues = parsed.issues;
        }
    }

    fn push_hunk(&mut self, kind: HunkKind, previous: String, inserted: String, at: Option<usize>) {
        if previous == inserted {
            return;
        }
        let id = self.next_hunk;
        self.next_hunk += 1;
        self.hunks.push(Hunk {
            id,
            kind,
            previous,
            inserted,
            at,
        });
    }

    fn reconcile_hunks(&mut self) {
        let text = self.serialize();
        let before = self.hunks.len();
        self.hunks
            .retain(|hunk| hunk.inserted.is_empty() || text.contains(&hunk.inserted));
        if self.hunks.len() != before {
            self.notices
                .push("kept your text where it overlapped an agent edit".into());
        }
    }

    fn user_serialized(&self) -> String {
        let mut text = self.serialize();
        for hunk in self.hunks.iter().rev() {
            if hunk.inserted.is_empty() {
                continue;
            }
            if let Some(next) = revert_span(&text, hunk) {
                text = next;
            }
        }
        text
    }

    fn address_views(&self, addresses: &[String]) -> Vec<AddressView> {
        addresses
            .iter()
            .map(|raw| {
                let email = email_of(raw).unwrap_or_default();
                let fresh = !email.is_empty() && !self.known.contains(&email);
                AddressView {
                    raw: raw.clone(),
                    email,
                    fresh,
                }
            })
            .collect()
    }

    fn checkpoint(&self) -> Checkpoint {
        Checkpoint {
            fields: self.fields.clone(),
            body: self.body.clone(),
            hunks: self.hunks.clone(),
            notices: self.notices.clone(),
        }
    }

    fn bump(&mut self) {
        self.revision = self.revision.saturating_add(1);
    }
}

impl DraftSession {
    fn load_hunks(&mut self) {
        let Ok(text) =
            std::fs::read_to_string(workspace::control_dir(&self.folder).join("hunks.json"))
        else {
            return;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
            return;
        };
        let Some(items) = value.as_array() else {
            return;
        };
        self.hunks.clear();
        for item in items {
            let Some(id) = item.get("id").and_then(serde_json::Value::as_u64) else {
                continue;
            };
            let kind = match item.get("kind").and_then(serde_json::Value::as_str) {
                Some("recipients") => HunkKind::Recipients,
                Some("subject") => HunkKind::Subject,
                Some("attachments") => HunkKind::Attachments,
                _ => HunkKind::Body,
            };
            self.hunks.push(Hunk {
                id,
                kind,
                previous: item
                    .get("previous")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                inserted: item
                    .get("inserted")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                at: item
                    .get("at")
                    .and_then(serde_json::Value::as_u64)
                    .map(|at| at as usize),
            });
        }
        self.next_hunk = self.hunks.iter().map(|hunk| hunk.id).max().unwrap_or(0) + 1;
    }

    fn save_hunks(&self) {
        let items: Vec<serde_json::Value> = self
            .hunks
            .iter()
            .map(|hunk| {
                let kind = match hunk.kind {
                    HunkKind::Body => "body",
                    HunkKind::Recipients => "recipients",
                    HunkKind::Subject => "subject",
                    HunkKind::Attachments => "attachments",
                };
                serde_json::json!({
                    "id": hunk.id,
                    "kind": kind,
                    "previous": hunk.previous,
                    "inserted": hunk.inserted,
                    "at": hunk.at,
                })
            })
            .collect();
        let control = workspace::control_dir(&self.folder);
        let _ = std::fs::create_dir_all(&control);
        let _ = std::fs::write(
            control.join("hunks.json"),
            serde_json::to_string(&items).unwrap_or_else(|_| "[]".into()),
        );
    }
}

fn span_after_edit(text: &str, inserted: &str, at: usize) -> Option<usize> {
    if text.get(at..at + inserted.len()) == Some(inserted) {
        return Some(at);
    }
    text.match_indices(inserted)
        .map(|(index, _)| index)
        .min_by_key(|index| index.abs_diff(at))
}

fn revert_span(text: &str, hunk: &Hunk) -> Option<String> {
    if let Some(at) = hunk.at
        && text.get(at..at + hunk.inserted.len()) == Some(hunk.inserted.as_str())
    {
        let mut next = text.to_string();
        next.replace_range(at..at + hunk.inserted.len(), &hunk.previous);
        return Some(next);
    }
    if text.matches(hunk.inserted.as_str()).count() == 1 {
        return Some(text.replacen(&hunk.inserted, &hunk.previous, 1));
    }
    None
}

fn referenced_file_hashes(folder: &Path, fields: &DraftFields, body: &str) -> Vec<(String, u64)> {
    let (files, _) = workspace::list_closure(folder, fields, body);
    let mut stamp = Vec::new();
    for file in files {
        let Some(hash) = hash_file(folder, &file.path) else {
            continue;
        };
        stamp.push((file.path, hash));
    }
    stamp.sort();
    stamp
}

/// Hash the file bytes. Length and mtime are not part of the stamp.
fn hash_file(folder: &Path, relative: &str) -> Option<u64> {
    let path = workspace::resolve_inside(folder, relative).ok()?;
    let mut file = std::fs::File::open(path).ok()?;
    let mut hash = 0xcbf29ce484222325u64;
    let mut buf = [0u8; 8192];
    loop {
        let read = std::io::Read::read(&mut file, &mut buf).ok()?;
        if read == 0 {
            break;
        }
        for byte in &buf[..read] {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
    }
    Some(hash)
}

fn attachment_stamp(folder: &Path, fields: &DraftFields, body: &str) -> Vec<(String, u64, u128)> {
    let (files, _) = workspace::list_closure(folder, fields, body);
    let mut stamp = Vec::new();
    for file in files {
        let mtime = std::fs::metadata(folder.join(&file.path))
            .ok()
            .and_then(|meta| meta.modified().ok())
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        stamp.push((file.path, file.len, mtime));
    }
    stamp.sort();
    stamp
}

fn hunk_view(hunk: &Hunk, text: &str) -> Option<HunkView> {
    if hunk.inserted.is_empty() {
        return None;
    }
    let start = text.find(&hunk.inserted)?;
    let label = match hunk.kind {
        HunkKind::Recipients => "agent changed recipients",
        HunkKind::Subject => "agent changed the subject",
        HunkKind::Attachments => "agent changed attachments",
        HunkKind::Body => "agent edit",
    };
    Some(HunkView {
        id: hunk.id,
        kind: hunk.kind,
        start,
        end: start + hunk.inserted.len(),
        label: label.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::super::frontmatter::AttachmentRef;
    use super::*;
    use std::fs;

    fn temp_session(body: &str) -> (PathBuf, DraftSession) {
        let root = std::env::temp_dir().join(format!(
            "fell-session-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let fields = DraftFields {
            from: "jon@example.com".into(),
            to: vec!["ada@example.com".into()],
            subject: "Hello".into(),
            in_reply_to: Some("<abc@example.com>".into()),
            ..DraftFields::default()
        };
        let folder =
            workspace::materialize_in(&root, "d1", &fields, body, &AccountId("local".into()))
                .unwrap();
        let session =
            DraftSession::create(folder, "d1", AccountId("local".into()), fields, body).unwrap();
        (root, session)
    }

    #[test]
    fn request_send_does_not_produce_a_message() {
        let (_root, mut session) = temp_session("Hello\n");
        let transmitted = session.request_send();
        assert!(!transmitted);
        assert!(session.is_open());
        assert!(session.try_agent_delete().is_err());
        session.mark_seen();
        let outgoing = session.prepare_send().unwrap();
        assert!(outgoing.mime.windows(4).any(|window| window == b"From"));
        assert!(
            !String::from_utf8(outgoing.mime)
                .unwrap()
                .contains("fixture-sent")
        );
    }

    #[test]
    fn a_fast_script_settles_into_one_update() {
        let (_root, mut session) = temp_session("alpha\n");
        let base = session.serialize();
        let mut latest = String::new();
        for (step, word) in ["one", "two", "three"].into_iter().enumerate() {
            latest = base.replace("alpha", word);
            session.push_disk(Some(latest.clone()), step as u64 * 20);
            assert!(!session.poll_disk(step as u64 * 20));
        }
        assert!(latest.contains("three"));
        assert!(session.poll_disk(20 * 2 + DEBOUNCE_MS));
        assert!(session.body().contains("three"));
        assert!(!session.body().contains("one\n") || session.body().contains("three"));
        assert_eq!(session.snapshot().unreviewed, 1);
    }

    #[test]
    fn the_user_wins_the_same_paragraph() {
        let (_root, mut session) = temp_session("Hello paragraph\n");
        session.set_body_from_user("Hello USER\n".into());
        session.end_user_change();
        let agent = session.serialize().replace("Hello USER", "Hello AGENT");
        let rewritten = session
            .last_written
            .replace("Hello paragraph", "Hello AGENT");
        session.push_disk(Some(rewritten), 0);
        assert!(session.poll_disk(DEBOUNCE_MS));
        assert!(session.body().contains("Hello USER"), "{}", session.body());
        assert!(!session.body().contains("Hello AGENT"));
        assert!(
            session
                .snapshot()
                .notices
                .iter()
                .any(|note| note.contains("kept your text") || note.contains("overlapped"))
        );
        let _ = agent;
    }

    #[test]
    fn an_editor_change_is_not_fsynced_until_the_debounce() {
        let (_root, mut session) = temp_session("alpha\n");
        let path = session.folder().join("draft.md");
        let before = fs::read_to_string(&path).unwrap();
        session.set_body_from_user("alpha beta\n".into());
        session.arm_outbound(0);
        assert!(!session.poll_outbound(149));
        assert_eq!(fs::read_to_string(&path).unwrap(), before);
        assert!(session.poll_outbound(150));
        let after = fs::read_to_string(&path).unwrap();
        assert!(after.contains("alpha beta"), "{after}");
        assert!(!session.poll_outbound(400));
    }

    #[test]
    fn opening_a_draft_treats_mailed_addresses_as_known() {
        let root = std::env::temp_dir().join(format!(
            "fell-known-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let db = root.join("mail.db");
        let store = crate::Store::open(&db).unwrap();
        let account = crate::Account {
            id: AccountId("a".into()),
            display_name: "Jon".into(),
            email: "jon@example.com".into(),
            backend_kind: crate::BackendKind::Bridge,
        };
        store.upsert_account(&account).unwrap();
        let message_id = crate::MessageId::new("sent-1@example.com");
        store
            .upsert_message(&crate::Message {
                account_id: account.id.clone(),
                headers: crate::MessageHeaders {
                    message_id: message_id.clone(),
                    proton_conversation_id: None,
                    in_reply_to: None,
                    references: Vec::new(),
                    from: crate::Address {
                        name: Some("Jon".into()),
                        email: "jon@example.com".into(),
                    },
                    subject: "Hello".into(),
                    date_unix: 1,
                },
                remote_id: crate::RemoteId("a:sent-1".into()),
                thread_id: crate::ThreadId::from_message_id(&message_id),
                snippet: "hi".into(),
                body_text: Some("hi".into()),
                unread: false,
                folders: vec!["Sent".into()],
                labels: Vec::new(),
                arrived_via: Vec::new(),
                to: vec![crate::Address {
                    name: None,
                    email: "ada@example.com".into(),
                }],
                cc: Vec::new(),
                bcc: Vec::new(),
                starred: false,
                has_attachment: false,
                archived: false,
                snooze_until: None,
                list_id: None,
            })
            .unwrap();
        let fields = DraftFields {
            from: "jon@example.com".into(),
            to: vec!["ada@example.com".into(), "new@example.com".into()],
            cc: vec!["jon@example.com".into()],
            subject: "Hello".into(),
            ..DraftFields::default()
        };
        let folder =
            workspace::materialize_in(&root, "d1", &fields, "body\n", &account.id).unwrap();
        let mut session =
            DraftSession::create(folder.clone(), "d1", account.id.clone(), fields, "body\n")
                .unwrap();
        session.bind_store(&db);
        session.flush().unwrap();
        let session = DraftSession::open_with_store(&folder, account.id, &store).unwrap();
        let snap = session.snapshot();
        let ada = snap
            .to
            .iter()
            .find(|address| address.email == "ada@example.com")
            .unwrap();
        let fresh = snap
            .to
            .iter()
            .find(|address| address.email == "new@example.com")
            .unwrap();
        let own = snap
            .cc
            .iter()
            .find(|address| address.email == "jon@example.com")
            .unwrap();
        assert!(!ada.fresh, "a mailed address is not new");
        assert!(!own.fresh, "the account address is not new");
        assert!(fresh.fresh, "an address this account has not mailed is new");
    }

    #[test]
    fn a_body_edit_does_not_reread_unchanged_attachments() {
        let (root, mut session) = temp_session("see ![chart](chart.png)\n");
        let db = root.join("mail.db");
        let store = crate::Store::open(&db).unwrap();
        store
            .upsert_account(&crate::Account {
                id: AccountId("local".into()),
                display_name: "Jon".into(),
                email: "jon@example.com".into(),
                backend_kind: crate::BackendKind::Bridge,
            })
            .unwrap();
        fs::write(session.folder().join("chart.png"), b"png-bytes").unwrap();
        session.bind_store(&db);
        session.flush().unwrap();
        store
            .conn
            .execute(
                "UPDATE compose_attachments SET bytes = ?1 WHERE draft_id = ?2",
                rusqlite::params![b"stale".as_slice(), session.id()],
            )
            .unwrap();
        session.set_body_from_user("edited ![chart](chart.png)\n".into());
        session.flush().unwrap();
        let loaded = store.load_compose(session.id()).unwrap().unwrap();
        assert!(loaded.body.contains("edited"), "{}", loaded.body);
        assert_eq!(loaded.files[0].bytes, b"stale");
    }

    #[test]
    fn a_flushed_keystroke_is_not_replaced_from_disk() {
        let (_root, mut session) = temp_session("Please send the report\n");
        let original = session.serialize();
        session.set_body_from_user("Please send the report today\n".into());
        session.flush().unwrap();
        let stale = original.replace("Please send the report", "Please file the report");
        session.push_disk(Some(stale), 0);
        assert!(session.poll_disk(DEBOUNCE_MS));
        assert!(
            session.body().contains("Please send the report today"),
            "{}",
            session.body()
        );
        assert!(!session.body().contains("file the report"));
    }

    #[test]
    fn a_flushed_keystroke_still_accepts_a_disjoint_file_edit() {
        let (_root, mut session) = temp_session("Please send the report\n\nThanks\n");
        let original = session.serialize();
        session.set_body_from_user("Please send the report today\n\nThanks\n".into());
        session.flush().unwrap();
        let agent = original.replace("Thanks", "Regards");
        session.push_disk(Some(agent), 0);
        assert!(session.poll_disk(DEBOUNCE_MS));
        assert!(session.body().contains("today"), "{}", session.body());
        assert!(session.body().contains("Regards"), "{}", session.body());
    }

    #[test]
    fn a_header_edit_is_a_recipient_or_subject_hunk() {
        let (_root, mut session) = temp_session("body\n");
        let agent = session
            .serialize()
            .replace("subject: \"Hello\"", "subject: \"Numbers\"");
        session.push_disk(Some(agent), 0);
        assert!(session.poll_disk(DEBOUNCE_MS));
        assert_eq!(session.fields().subject, "Numbers");
        assert!(
            session
                .snapshot()
                .hunks
                .iter()
                .any(|hunk| hunk.kind == HunkKind::Subject)
        );
    }

    #[test]
    fn deleting_the_file_restores_the_buffer() {
        let (_root, mut session) = temp_session("keep this body\n");
        session.push_disk(None, 0);
        assert!(session.poll_disk(DEBOUNCE_MS));
        assert_eq!(session.body(), "keep this body\n");
        let text = workspace::read_draft_file(session.folder()).unwrap();
        assert!(text.contains("keep this body"));
    }

    #[test]
    fn fell_ignores_its_own_write() {
        let (_root, mut session) = temp_session("same\n");
        let written = session.serialize();
        let revision = session.revision();
        session.push_disk(Some(written), 0);
        assert!(!session.poll_disk(DEBOUNCE_MS));
        assert_eq!(session.revision(), revision);
    }

    #[test]
    fn yaml_errors_keep_the_body_and_block_send() {
        let (_root, mut session) = temp_session("precious\n");
        session.push_disk(Some("this is not a draft\n".into()), 0);
        assert!(session.poll_disk(DEBOUNCE_MS));
        assert_eq!(session.body(), "precious\n");
        let text = "---\nfrom: jon@example.com\nto: [\"ada@example.com\"]\ncc: []\nbcc: []\nsubject: \"Hello\"\nin_reply_to: \"<abc@example.com>\"\nevil: 1\nattachments: []\n---\nprecious\n";
        session.push_disk(Some(text.into()), 1_000);
        assert!(session.poll_disk(1_000 + DEBOUNCE_MS));
        assert!(session.body().contains("precious"));
        session.mark_seen();
        assert!(session.prepare_send().is_err());
    }

    #[test]
    fn in_reply_to_edits_are_ignored() {
        let (_root, mut session) = temp_session("body\n");
        let agent = session.serialize().replace(
            "in_reply_to: \"<abc@example.com>\"",
            "in_reply_to: \"<other@example.com>\"",
        );
        session.push_disk(Some(agent), 0);
        assert!(session.poll_disk(DEBOUNCE_MS));
        assert_eq!(
            session.fields().in_reply_to.as_deref(),
            Some("<abc@example.com>")
        );
        assert!(
            session
                .snapshot()
                .notices
                .iter()
                .any(|note| note.contains("in_reply_to"))
        );
    }

    #[test]
    fn a_corrected_header_clears_the_old_parse_issue() {
        let (_root, mut session) = temp_session("body\n");
        let original = session.serialize();
        let bad = original.replace(
            "to: [\"ada@example.com\"]",
            "to: [\"not-an-email\"]\nevil: 1",
        );
        session.edit(session.revision(), &original, &bad).unwrap();
        session.mark_seen();
        assert!(session.prepare_send().is_err());
        session
            .set_addresses_from_user("to", "ada@example.com")
            .unwrap();
        session.mark_seen();
        assert!(
            session.prepare_send().is_ok(),
            "{:?}",
            session.snapshot().issues
        );
    }

    #[test]
    fn edit_does_not_rewrite_a_line_the_person_extended() {
        let (_root, mut session) = temp_session("Please send the report\n");
        session.set_body_from_user("Please send the report today\n".into());
        let err = session
            .edit(session.revision(), "send the report", "file the report")
            .unwrap_err();
        assert!(err.contains("today"), "{err}");
        assert!(session.body().contains("Please send the report today"));
        assert!(!session.body().contains("file the report"));
    }

    #[test]
    fn edit_draft_rejects_text_the_user_changed() {
        let (_root, mut session) = temp_session("alpha paragraph\n");
        session.set_body_from_user("user paragraph\n".into());
        let err = session
            .edit(1, "alpha paragraph", "agent paragraph")
            .unwrap_err();
        assert!(err.contains("user paragraph"));
        assert!(session.body().contains("user paragraph"));
    }

    #[test]
    fn user_changes_are_coalesced_for_the_agent() {
        let (_root, mut session) = temp_session("alpha\n");
        assert!(session.take_user_changes("bot").is_empty());
        session.set_body_from_user("alpha!\n".into());
        session.set_body_from_user("alpha!!\n".into());
        let diff = session.take_user_changes("bot");
        assert!(diff.contains("alpha!!"), "{diff}");
        assert!(!diff.contains("alpha!\n-") && diff.matches("alpha!").count() >= 1);
    }

    #[test]
    fn replace_body_is_one_hunk() {
        let (_root, mut session) = temp_session("short\n");
        session
            .replace_body("a full rewrite of the letter\n")
            .unwrap();
        assert_eq!(session.snapshot().unreviewed, 1);
        assert_eq!(session.snapshot().hunks.len(), 1);
    }

    #[test]
    fn unreferenced_files_are_not_in_the_outgoing_message() {
        let (root, mut session) = temp_session("![chart](chart.png)\n");
        fs::write(session.folder().join("chart.png"), b"png").unwrap();
        fs::write(session.folder().join("secret.txt"), b"nope").unwrap();
        session.fields.attachments.push(AttachmentRef {
            path: "chart.png".into(),
            name: None,
            line: 1,
        });
        // The image is referenced. secret.txt is not.
        session.bump();
        session.mark_seen();
        let outgoing = session.prepare_send().unwrap();
        let mime = String::from_utf8(outgoing.mime).unwrap();
        assert!(mime.contains("chart.png"));
        assert!(!mime.contains("secret.txt"));
        assert!(!mime.contains("nope"));
        let _ = root;
    }

    #[test]
    fn from_outside_the_account_blocks_send() {
        let (_root, mut session) = temp_session("body\n");
        session
            .edit(session.revision(), "jon@example.com", "evil@example.com")
            .unwrap();
        session.mark_seen();
        let err = session.prepare_send().unwrap_err();
        assert!(err.contains("from"), "{err}");
    }

    #[test]
    fn offline_edit_keeps_the_reply_lock_and_records_a_hunk() {
        let (root, mut session) = temp_session("body\n");
        let db = root.join("mail.db");
        let store = crate::Store::open(&db).unwrap();
        store
            .upsert_account(&crate::Account {
                id: AccountId("local".into()),
                display_name: "Jon".into(),
                email: "jon@example.com".into(),
                backend_kind: crate::BackendKind::Bridge,
            })
            .unwrap();
        session.bind_store(&db);
        session.owner = "agent:test".into();
        session.flush().unwrap();
        let folder = session.folder().to_path_buf();
        drop(session);
        let mut again =
            DraftSession::open_with_store(&folder, AccountId("local".into()), &store).unwrap();
        again
            .edit(
                again.revision(),
                "in_reply_to: \"<abc@example.com>\"",
                "in_reply_to: \"<evil@example.com>\"",
            )
            .unwrap();
        again
            .edit(again.revision(), "ada@example.com", "new@example.com")
            .unwrap();
        again.flush().unwrap();
        let text = fs::read_to_string(folder.join("draft.md")).unwrap();
        assert!(text.contains("<abc@example.com>"), "{text}");
        assert!(!text.contains("<evil@example.com>"), "{text}");
        let mismatch = DraftSession::open_with_store(&folder, AccountId("other".into()), &store);
        match mismatch {
            Ok(_) => panic!("opened with the wrong account"),
            Err(err) => assert!(err.contains("account"), "{err}"),
        }
        let reopened =
            DraftSession::open_with_store(&folder, AccountId("local".into()), &store).unwrap();
        assert_eq!(reopened.account_id().as_str(), "local");
        assert_eq!(
            reopened.fields().in_reply_to.as_deref(),
            Some("<abc@example.com>")
        );
        assert!(reopened.snapshot().unreviewed >= 1);
        assert!(text.contains("new@example.com"), "{text}");
        let _ = root;
    }

    #[test]
    fn open_refuses_a_folder_with_no_mailbox_record() {
        let (root, session) = temp_session("body\n");
        let folder = session.folder().to_path_buf();
        drop(session);
        let err = match DraftSession::open_folder(&folder, AccountId("local".into())) {
            Ok(_) => panic!("opened a folder with no mailbox record"),
            Err(err) => err,
        };
        assert!(err.contains("mailbox"), "{err}");
        let _ = root;
    }

    #[test]
    fn control_files_are_outside_the_agent_folder() {
        let (root, session) = temp_session("body\n");
        let folder = session.folder().to_path_buf();
        drop(session);
        assert!(!folder.join("account").exists());
        assert!(!folder.join("reply.lock").exists());
        assert!(!folder.join("identities").exists());
        std::fs::write(folder.join("account"), "not-the-account\n").unwrap();
        std::fs::write(folder.join("reply.lock"), "<evil@example.com>\n").unwrap();
        std::fs::write(folder.join("identities"), "evil@example.com\n").unwrap();
        let db = root.join("mail.db");
        let store = crate::Store::open(&db).unwrap();
        store
            .upsert_account(&crate::Account {
                id: AccountId("local".into()),
                display_name: "Jon".into(),
                email: "jon@example.com".into(),
                backend_kind: crate::BackendKind::Bridge,
            })
            .unwrap();
        let mut live = DraftSession::create(
            folder.clone(),
            "d1",
            AccountId("local".into()),
            DraftFields {
                from: "jon@example.com".into(),
                to: vec!["ada@example.com".into()],
                subject: "Hello".into(),
                in_reply_to: Some("<abc@example.com>".into()),
                ..DraftFields::default()
            },
            "body\n",
        )
        .unwrap();
        live.bind_store(&db);
        live.flush().unwrap();
        let opened = DraftSession::open_with_store(&folder, AccountId("local".into()), &store);
        let opened = match opened {
            Ok(session) => session,
            Err(err) => panic!("{err}"),
        };
        assert_eq!(opened.account_id().as_str(), "local");
        assert_eq!(
            opened.fields().in_reply_to.as_deref(),
            Some("<abc@example.com>")
        );
        assert!(opened.identities.contains("jon@example.com"));
        assert!(!opened.identities.contains("evil@example.com"));
        let _ = root;
    }

    #[test]
    fn reject_reverts_the_edited_range_not_an_earlier_copy() {
        let (_root, mut session) = temp_session("alpha\nbeta alpha\n");
        session
            .edit(session.revision(), "beta alpha", "alpha")
            .unwrap();
        assert_eq!(session.body(), "alpha\nalpha\n");
        let hunk = session.snapshot().hunks[0].id;
        assert!(session.reject_hunk(hunk));
        assert_eq!(session.body(), "alpha\nbeta alpha\n");
    }

    #[test]
    fn send_blocks_until_the_painted_revision_is_marked_seen() {
        let (_root, session) = temp_session("body\n");
        let err = session.prepare_send().unwrap_err();
        assert!(err.contains("on screen"), "{err}");
    }

    #[test]
    fn undo_removes_an_agent_edit() {
        let (_root, mut session) = temp_session("alpha\n");
        session.edit(session.revision(), "alpha", "beta").unwrap();
        assert!(session.body().contains("beta"));
        assert!(session.undo());
        assert!(session.body().contains("alpha"));
    }

    #[test]
    fn send_blocks_when_a_painted_attachment_changes() {
        let (_root, mut session) = temp_session("![chart](chart.png)\n");
        fs::write(session.folder().join("chart.png"), b"one").unwrap();
        session.mark_seen();
        assert!(session.prepare_send().is_ok());
        fs::write(session.folder().join("chart.png"), b"replaced").unwrap();
        session.mark_seen();
        let err = session.prepare_send().unwrap_err();
        assert!(err.contains("attachment"), "{err}");
    }

    #[test]
    fn a_text_edit_does_not_accept_swapped_attachment_bytes() {
        let (_root, mut session) = temp_session("![chart](chart.png)\n");
        let path = session.folder().join("chart.png");
        fs::write(&path, b"one").unwrap();
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        session.mark_seen();
        assert!(session.prepare_send().is_ok());
        fs::write(&path, b"two").unwrap();
        let file = fs::File::options().write(true).open(&path).unwrap();
        file.set_modified(modified).unwrap();
        session.set_body_from_user("![chart](chart.png)\n\nmore\n".into());
        session.mark_seen();
        let err = session.prepare_send().unwrap_err();
        assert!(err.contains("attachment"), "{err}");
        session.set_body_from_user("no chart\n".into());
        session.mark_seen();
        session.set_body_from_user("![chart](chart.png)\n".into());
        session.mark_seen();
        assert!(session.prepare_send().is_ok());
    }
}
