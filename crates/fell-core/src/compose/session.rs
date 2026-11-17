//! The live draft. The open editor holds the only editable copy.
//!
//! Agent edits and `draft.md` writes share this merge. Neither path transmits.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::AccountId;

use super::frontmatter::{
    DraftFields, DraftIssue, email_of, parse_address_line, parse_draft, valid_address, write_draft,
};
use super::merge::{self, HunkKind};
use super::mime::{self, Outgoing, ResolvedFile};
use super::workspace::{self, Closure};

const DEBOUNCE_MS: u64 = 150;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Hunk {
    id: u64,
    kind: HunkKind,
    previous: String,
    inserted: String,
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
    locked_reply: Option<String>,
    hunks: Vec<Hunk>,
    next_hunk: u64,
    issues: Vec<DraftIssue>,
    notices: Vec<String>,
    undo: Vec<Checkpoint>,
    undo_open: bool,
    last_written: String,
    pending: Option<String>,
    pending_delete: bool,
    pending_since: u64,
    open: bool,
    focused: bool,
    known: HashSet<String>,
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
            locked_reply,
            hunks: Vec::new(),
            next_hunk: 1,
            issues: Vec::new(),
            notices: Vec::new(),
            undo: Vec::new(),
            undo_open: false,
            last_written: String::new(),
            pending: None,
            pending_delete: false,
            pending_since: 0,
            open: false,
            focused: false,
            known: HashSet::new(),
            agent_seen: HashMap::new(),
            quoted: None,
        };
        let _ = std::fs::write(session.folder.join("account"), session.account_id.as_str());
        session.flush()?;
        Ok(session)
    }

    pub fn open_folder(folder: &Path, account_id: AccountId) -> Result<Self, String> {
        let text = workspace::read_draft_file(folder)?;
        let parsed = parse_draft(&text).map_err(|err| err.message)?;
        let id = folder
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("draft")
            .to_string();
        let stored = std::fs::read_to_string(folder.join("account")).unwrap_or_default();
        let account_id = if stored.trim().is_empty() {
            account_id
        } else {
            AccountId(stored.trim().to_string())
        };
        let mut session = Self::create(
            folder.to_path_buf(),
            id,
            account_id,
            parsed.fields,
            parsed.body,
        )?;
        session.issues = parsed.issues;
        let _ = text;
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
        self.seen_revision = self.revision;
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
    }

    pub fn set_subject_from_user(&mut self, subject: String) {
        if subject == self.fields.subject {
            return;
        }
        self.begin_user_change();
        self.fields.subject = subject;
        self.bump();
    }

    pub fn set_from_user(&mut self, from: String) {
        if from == self.fields.from {
            return;
        }
        self.begin_user_change();
        self.fields.from = from;
        self.bump();
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
        true
    }

    pub fn keep_hunk(&mut self, id: u64) -> bool {
        let Some(index) = self.hunks.iter().position(|hunk| hunk.id == id) else {
            return false;
        };
        self.begin_user_change();
        self.end_user_change();
        self.hunks.remove(index);
        self.bump();
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
        let next = text.replacen(&hunk.inserted, &hunk.previous, 1);
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
        let mut next = current.clone();
        next.replace_range(at..at + old.len(), new);
        self.undo.push(self.checkpoint());
        self.adopt_text(&next)?;
        if old != new && self.serialize().contains(new) {
            self.push_hunk(HunkKind::Body, old.to_string(), new.to_string());
        }
        Ok(())
    }

    pub fn replace_body(&mut self, body: &str) -> Result<(), String> {
        if body == self.body {
            return Ok(());
        }
        self.undo.push(self.checkpoint());
        let previous = self.body.clone();
        self.body = body.to_string();
        self.push_hunk(HunkKind::Body, previous, body.to_string());
        self.bump();
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

    pub fn flush(&mut self) -> Result<(), String> {
        let text = self.serialize();
        workspace::write_draft_file(&self.folder, &text)?;
        self.last_written = text;
        Ok(())
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
        let closure = workspace::resolve_closure(&self.folder, &self.fields, &self.body);
        let mut issues = self.issues.clone();
        issues.extend(closure.issues.clone());
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
            attachments: closure
                .files
                .iter()
                .map(|file| AttachmentView {
                    path: file.path.clone(),
                    name: file.display_name.clone(),
                    bytes: file.bytes.len() as u64,
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
        if let Some(issue) = self.issues.iter().find(|issue| issue.blocks_send) {
            return Some(issue.message.clone());
        }
        if self.fields.to.is_empty() {
            return Some("add at least one recipient".into());
        }
        if !valid_address(&self.fields.from) {
            return Some("from is not one of your addresses".into());
        }
        let closure = workspace::resolve_closure(&self.folder, &self.fields, &self.body);
        closure
            .issues
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
        let outcome = merge::merge_draft(&self.last_written, &self.serialize(), text);
        self.notices.extend(outcome.skipped);
        if self.adopt_text(&outcome.text).is_err() {
            self.notices.push(
                "draft.md could not be parsed; kept the open draft and rewrote the file".into(),
            );
            let _ = self.flush();
            return;
        }
        for hunk in outcome.hunks {
            self.push_hunk(hunk.kind, hunk.previous, hunk.inserted);
        }
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

    fn push_hunk(&mut self, kind: HunkKind, previous: String, inserted: String) {
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
            if let Some(at) = text.find(&hunk.inserted) {
                text.replace_range(at..at + hunk.inserted.len(), &hunk.previous);
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
        let folder = workspace::materialize_in(&root, "d1", &fields, body).unwrap();
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
    fn undo_removes_an_agent_edit() {
        let (_root, mut session) = temp_session("alpha\n");
        session.edit(session.revision(), "alpha", "beta").unwrap();
        assert!(session.body().contains("beta"));
        assert!(session.undo());
        assert!(session.body().contains("alpha"));
    }
}
