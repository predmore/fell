//! The live draft. The open editor holds the only editable copy.
//!
//! Agent edits and `draft.md` writes share this merge. Neither path transmits.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::{AccountId, Identity};

use super::digest::{self, Digests, Known};
use super::frontmatter::{
    DraftFields, DraftIssue, ParsedDraft, email_of, parse_address_line, parse_draft, write_draft,
};
use super::merge::{self, HunkKind, MergeOutcome};
use super::mime::{self, Outgoing, ResolvedFile};
use super::persist::{self, StoredReview};
use super::reply::ReplyParts;
use super::review::{self, Hunk};
use super::workspace::{self, Closure, FileStamp, ListedFile};
use super::writer::{
    DraftWriter, MailboxSave, OwnWrites, SaveCache, SaveError, SaveJob, SaveWaiter,
};

const DEBOUNCE_MS: u64 = 150;

const CHANGED_ATTACHMENT: &str = "an attachment changed after it was shown";
const CHECKING_ATTACHMENT: &str =
    "still checking an attachment that was just shown; try sending again in a moment";

#[derive(Debug, Clone)]
struct Checkpoint {
    fields: DraftFields,
    body: String,
    managed_signature: Option<String>,
    hunks: Vec<Hunk>,
    notices: Vec<String>,
}

/// The mailbox and envelope data for recording a confirmed send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentCopy {
    store: PathBuf,
    draft_id: String,
    account_id: AccountId,
    message_id: crate::MessageId,
    bcc: Vec<crate::Address>,
    visible: Vec<crate::Address>,
}

impl SentCopy {
    /// Save and thread the confirmed MIME and queue its server copy if enabled.
    pub fn save(
        &self,
        mime: &[u8],
        append: bool,
    ) -> crate::Result<(crate::ThreadId, crate::Message)> {
        crate::store::retry_sent_write(|| crate::Store::open(&self.store))?.record_sent_message(
            &self.account_id,
            &self.draft_id,
            &self.message_id,
            &self.bcc,
            mime,
            append,
        )
    }

    /// Remember the envelope's Bcc and, for confirmed sends without a
    /// local Sent copy, keep the recipients available for autocomplete.
    pub fn remember_recipients(&self, confirmed: bool) -> crate::Result<()> {
        if !confirmed && self.bcc.is_empty() {
            return Ok(());
        }
        let recipients: Vec<_> = self.visible.iter().chain(&self.bcc).cloned().collect();
        let sent_at = crate::outbox::now_unix();
        crate::store::retry_sent_write(|| {
            let store = crate::Store::open(&self.store)?;
            if !self.bcc.is_empty() {
                store.record_sent_bcc(&self.account_id, &self.message_id, &self.bcc)?;
            }
            if confirmed {
                store.record_sent_correspondents(
                    &self.account_id,
                    &self.message_id,
                    &recipients,
                    sent_at,
                )?;
            }
            Ok(())
        })
    }
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
    /// The text this change replaced, so the person can judge it.
    pub previous: String,
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
    /// The agent whose `request_send` opened this draft, if one did.
    pub send_requested_by: Option<String>,
    /// The message this draft replies to, as "sender · subject · date",
    /// when the mailbox has it.
    pub reply_context: Option<ReplyContext>,
    /// The reply opened with recipients withheld pending verification.
    pub reply_to_unverified: bool,
    /// What a forward carries, read from its header block.
    pub forwarded: Option<super::reply::ForwardSummary>,
    /// An agent wrote this draft (`create_draft`), so every word of it,
    /// the first body too, is the agent's and not yet the person's.
    pub agent_authored: bool,
}

/// The parent message of a reply, read from the mailbox when the draft opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplyContext {
    pub sender: String,
    /// The sender's address, shown when the reply goes somewhere else.
    pub sender_email: String,
    pub arrived_via: Vec<String>,
    /// Where the original asked replies to go, lowercased. `None` means
    /// the original's Reply-To is unverified.
    pub reply_to: Option<Vec<String>>,
    pub subject: String,
    pub date_unix: i64,
}

/// Why the editor will not send a draft (again).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendLock {
    /// A send of this draft is talking to the server.
    InFlight,
    /// The server accepted this draft.
    Sent,
    /// The connection failed after the server had the whole message.
    MaybeSent,
}

impl Snapshot {
    /// Where the body starts in [`Self::text`], the serialized draft.
    pub fn body_start(&self) -> Option<usize> {
        let tail = usize::from(!self.body.is_empty() && !self.body.ends_with('\n'));
        let start = self.text.len().checked_sub(self.body.len() + tail)?;
        self.text[start..]
            .starts_with(self.body.as_str())
            .then_some(start)
            .filter(|start| self.text.is_char_boundary(*start))
    }

    /// A byte range of [`Self::text`] (a hunk's) as a range of the body,
    /// cut to the part inside it. `None` when none of it is in the body.
    pub fn body_range(&self, start: usize, end: usize) -> Option<(usize, usize)> {
        let base = self.body_start()?;
        let start = start.max(base) - base;
        let end = end.min(base + self.body.len()).checked_sub(base)?;
        (start < end).then_some((start, end))
    }

    /// Why this draft cannot be sent again, when a send went out or may have.
    pub fn send_lock_reason(&self) -> Option<&'static str> {
        match self.send_block.as_deref() {
            Some(reason) if reason == SendLock::Sent.reason() => Some(SendLock::Sent.reason()),
            Some(reason) if reason == SendLock::MaybeSent.reason() => {
                Some(SendLock::MaybeSent.reason())
            }
            _ => None,
        }
    }
}

impl SendLock {
    pub fn reason(self) -> &'static str {
        match self {
            Self::InFlight => "this draft is being sent",
            Self::Sent => "this draft was sent",
            Self::MaybeSent => "this draft may already have been sent; check Sent",
        }
    }
}

pub struct DraftSession {
    id: String,
    folder: PathBuf,
    account_id: AccountId,
    fields: DraftFields,
    body: String,
    revision: u64,
    seen_revision: u64,
    /// Referenced files the person has accepted: path and file version.
    /// A later text edit does not replace a version that is already recorded.
    painted_files: Option<Vec<(String, FileStamp)>>,
    /// The next paint may replace accepted attachment versions.
    accept_files: bool,
    /// SHA-256 of attachment versions, hashed off the paint path.
    digests: Digests,
    /// The attachment bytes [`Self::prepare_send`] checked, and the revision
    /// it checked them for.
    prepared: Mutex<Option<(u64, Vec<ResolvedFile>)>>,
    locked_reply: Option<String>,
    /// Agent changes not yet reviewed. Ranges point into [`Self::serialize`]
    /// and never overlap.
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
    /// The agent that last asked the person to review and send this draft.
    send_requested_by: Option<String>,
    reply_context: Option<ReplyContext>,
    /// When the latest unflushed editor change was armed. `None` when the file is current.
    outbound_since: Option<u64>,
    pub(super) server_save: super::server::Debounce,
    /// Synchronous saves: the attachment stamp last copied into the mailbox.
    save_cache: SaveCache,
    /// Saves run here, off the lock the editor paints from, once attached.
    writer: Option<DraftWriter>,
    /// Texts queued on `writer` that `draft.md` may still show.
    own_writes: OwnWrites,
    pending: Option<String>,
    /// [`Self::disk_generation`] when `pending` was read.
    pending_seen: u64,
    pending_delete: bool,
    pending_since: u64,
    open: bool,
    focused: bool,
    known: HashSet<String>,
    identities: Vec<Identity>,
    managed_signature: Option<String>,
    owner: String,
    agent_seen: HashMap<String, String>,
    /// References and the quoted original of a reply or forward. Fell sets
    /// these; the draft file does not carry them.
    reply: ReplyParts,
    /// Set before a send starts and kept after it may have gone out, so no
    /// second press or reopened view sends it again. The mailbox holds the
    /// record a restart reads; see [`Self::begin_send`].
    send_lock: Option<SendLock>,
    send_guard: Option<std::sync::Arc<std::fs::File>>,
}

impl DraftSession {
    pub fn create(
        folder: PathBuf,
        id: impl Into<String>,
        account_id: AccountId,
        fields: DraftFields,
        body: impl Into<String>,
    ) -> Result<Self, String> {
        let mut session = Self::unsaved(folder, id.into(), account_id, fields, body.into());
        session.flush()?;
        Ok(session)
    }

    /// Create a folder-only fixture under a test's private root. Its mailbox
    /// path stays absent unless the test binds a fixture store later.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn testing_create_local(
        folder: PathBuf,
        id: impl Into<String>,
        account_id: AccountId,
        fields: DraftFields,
        body: impl Into<String>,
    ) -> Result<Self, String> {
        let unused_store = folder.with_file_name("unused-mailbox.db");
        let mut session = Self::unsaved(folder, id.into(), account_id, fields, body.into());
        session.bind_store(unused_store);
        session.flush()?;
        Ok(session)
    }

    /// Start draft `id` under `root`: its row in the mailbox at `store`
    /// first, then its folder. A crash in between leaves a row, which the
    /// drafts list shows and opening writes a folder for, never a folder no
    /// list knows of. A folder that cannot be made takes the row with it.
    pub fn start_in(
        root: &Path,
        id: &str,
        account_id: AccountId,
        fields: DraftFields,
        body: &str,
        store: &Path,
    ) -> Result<PathBuf, String> {
        Self::start_reply_in(
            root,
            id,
            account_id,
            fields,
            body,
            &ReplyParts::default(),
            store,
        )
    }

    /// [`Self::start_in`] for a reply or forward: its References and quoted
    /// original are kept in the mailbox row with it.
    pub fn start_reply_in(
        root: &Path,
        id: &str,
        account_id: AccountId,
        fields: DraftFields,
        body: &str,
        reply: &ReplyParts,
        store: &Path,
    ) -> Result<PathBuf, String> {
        let identities = email_of(&fields.from).into_iter().collect();
        Self::start_stored_in(
            root,
            persist::StoredCompose {
                id: id.to_string(),
                account_id,
                fields,
                body: body.to_string(),
                files: Vec::new(),
                identities,
                managed_signature: None,
                owner: String::new(),
                reply: reply.clone(),
            },
            store,
        )
    }

    /// Reserve a new compose row before making its editable folder.
    pub fn start_stored_in(
        root: &Path,
        draft: persist::StoredCompose,
        store: &Path,
    ) -> Result<PathBuf, String> {
        workspace::validate_id(&draft.id)?;
        if !store.is_file() {
            return Err("the mailbox file is missing".into());
        }
        let folder = root.join(&draft.id);
        let review = StoredReview {
            text: write_draft(&draft.fields, &draft.body),
            hunks: "[]".into(),
            next_hunk: 1,
        };
        super::writer::persist_mailbox(
            &folder,
            MailboxSave {
                store_path: Some(store.to_path_buf()),
                draft: draft.clone(),
                review,
            },
            &mut SaveCache::default(),
        )?;
        match workspace::materialize_in(
            root,
            &draft.id,
            &draft.fields,
            &draft.body,
            &draft.account_id,
        ) {
            Ok(folder) => Ok(folder),
            Err(err) => {
                let _ = std::fs::remove_dir_all(&folder);
                let _ = std::fs::remove_dir_all(workspace::control_dir(&folder));
                if let Ok(opened) = crate::Store::open(store) {
                    let _ = opened.delete_compose(&draft.id);
                }
                Err(err)
            }
        }
    }

    fn unsaved(
        folder: PathBuf,
        id: String,
        account_id: AccountId,
        fields: DraftFields,
        body: String,
    ) -> Self {
        let locked_reply = fields.in_reply_to.clone();
        let mut session = Self {
            id,
            folder,
            account_id,
            fields,
            body,
            revision: 1,
            seen_revision: 0,
            painted_files: None,
            accept_files: false,
            digests: Digests::default(),
            prepared: Mutex::new(None),
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
            send_requested_by: None,
            reply_context: None,
            outbound_since: None,
            server_save: Default::default(),
            save_cache: SaveCache::default(),
            writer: None,
            own_writes: OwnWrites::default(),
            pending: None,
            pending_seen: 0,
            pending_delete: false,
            pending_since: 0,
            open: false,
            focused: false,
            known: HashSet::new(),
            identities: Vec::new(),
            managed_signature: None,
            owner: String::new(),
            agent_seen: HashMap::new(),
            reply: ReplyParts::default(),
            send_lock: None,
            send_guard: None,
        };
        if let Some(email) = email_of(&session.fields.from) {
            session.identities.push(address_only(email));
        }
        session.settled = session.serialize();
        session
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
        let review = store
            .load_compose_review(&id)
            .map_err(|err| err.to_string())?
            .filter(|review| review_matches_row(&review.text, &stored));
        let mut session = Self::unsaved(
            folder.to_path_buf(),
            id,
            stored.account_id,
            parsed.fields,
            parsed.body,
        );
        session.identities = stored.identities.into_iter().map(address_only).collect();
        let previous_from = stored.fields.from;
        let verified =
            crate::account_config::configured_sender(&session.account_id, &session.fields.from);
        session.managed_signature = stored.managed_signature;
        session.check_signature();
        if let Ok(identity) = verified {
            if email_of(&previous_from) != email_of(&session.fields.from) {
                session.swap_signature(signature_block(&identity));
            }
            session.fields.from = identity.from_header();
        } else if session.fields.from != previous_from {
            session.fields.from =
                crate::account_config::configured_sender(&session.account_id, &previous_from)
                    .map_or(previous_from, |identity| identity.from_header());
            session
                .notices
                .push("from is not one of your addresses; the edit was ignored".into());
        }
        session.owner = stored.owner;
        session.locked_reply = stored.fields.in_reply_to.clone();
        session.fields.in_reply_to = session.locked_reply.clone();
        session.reply = stored.reply;
        if session.reply.references.is_empty()
            && let Some(parent) = session
                .locked_reply
                .as_deref()
                .filter(|id| !id.trim().is_empty())
        {
            // A reply started without its chain, such as an agent's: the
            // parent's own References, when the mailbox has the parent. If
            // it does not yet, the chain stays empty so a later open builds
            // it; a send meanwhile cites the parent alone.
            let found = store
                .message(session.account_id(), &crate::MessageId::new(parent))
                .ok()
                .flatten();
            if let Some(message) = found {
                // The rule a reply started here uses, so a long chain keeps
                // its root either way.
                session.reply.references = super::reply::reply_references(&message, parent);
            }
        }
        session.issues = parsed.issues;
        if let Some(path) = store.db_path() {
            session.store_path = Some(path.to_path_buf());
        }
        if let Ok(known) = store.mailed_addresses(session.account_id()) {
            session.known = known;
        }
        session.reply_context = session
            .locked_reply
            .as_deref()
            .filter(|id| !id.trim().is_empty())
            .and_then(|id| {
                store
                    .message_summary(session.account_id(), &crate::MessageId::new(id))
                    .ok()
                    .flatten()
            });
        match review {
            Some(review) => session.resume_review(review),
            None => session.import_legacy_hunks(),
        }
        session.send_lock = session.load_send_lock(store);
        session.settled = session.serialize();
        session.flush()?;
        Ok(session)
    }

    /// The send lock the mailbox holds for this draft. A draft first opened
    /// by this build reads the `send-state` file older builds kept in the
    /// control folder once, moves it into the mailbox and deletes it. After
    /// that the file is ignored: agents can write that folder.
    fn load_send_lock(&mut self, store: &crate::Store) -> Option<SendLock> {
        let legacy = workspace::control_dir(&self.folder).join(LEGACY_SEND_STATE);
        let record = match store.compose_send_record(&self.account_id, &self.id) {
            Ok(record) => record,
            Err(err) => {
                // Unknown is treated as "may have been sent".
                self.notices
                    .push(format!("could not read this draft's send record: {err}"));
                return Some(SendLock::MaybeSent);
            }
        };
        if let Some(lock) = record {
            let _ = std::fs::remove_file(&legacy);
            return lock;
        }
        let lock = read_legacy_send_state(&legacy);
        match store.migrate_compose_send(&self.account_id, &self.id, lock) {
            Ok(recorded) => {
                let _ = std::fs::remove_file(&legacy);
                recorded
            }
            Err(err) => {
                if lock.is_some() {
                    self.notices.push(format!(
                        "could not move this draft's send record into the mailbox: {err}"
                    ));
                }
                // The file is read again on the next open.
                lock
            }
        }
    }

    /// A draft saved before review state moved into the mailbox kept it in
    /// `hunks.json`, which agents can write. It is not imported: a hunk's
    /// `previous` text is what reject restores, so a forged file could plant
    /// an attacker's sentence behind a reject. The person is told to read the
    /// draft instead; the next save deletes the file.
    fn import_legacy_hunks(&mut self) {
        let path = workspace::control_dir(&self.folder).join("hunks.json");
        if path.is_file() {
            self.notices.push(
                "review marks from an older Fell were not kept; read this draft before you send it"
                    .into(),
            );
        }
    }

    /// Hunks come from the mailbox, never from the draft folder. Whatever
    /// `draft.md` gained since Fell last saved was written while no editor
    /// held the draft, so it is an agent change and is marked for review.
    fn resume_review(&mut self, review: StoredReview) {
        self.hunks = review::from_json(&review.hunks, &review.text);
        self.next_hunk = self
            .hunks
            .iter()
            .map(|hunk| hunk.id + 1)
            .max()
            .unwrap_or(1)
            .max(review.next_hunk);
        let current = self.serialize();
        if review.text == current {
            return;
        }
        let outcome = merge::merge_draft(&review.text, &review.text, &current);
        if !self.apply_agent_merge(&review.text, outcome) {
            review::carry(&mut self.hunks, &review.text, &current);
        }
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

    /// Paint `html` as the quoted original below a draft that quotes
    /// nothing, as the demo does. A reply's quote is sent in both parts, so
    /// it is never replaced here: the two parts would disagree.
    pub fn set_quote(&mut self, html: Option<String>) {
        if self.reply.quote_text.is_empty() {
            self.reply.quote_html = html.unwrap_or_default();
        }
    }

    /// References and the quoted original this draft sends with it.
    pub fn reply_parts(&self) -> &ReplyParts {
        &self.reply
    }

    /// The window painted this revision. Records which version of each
    /// referenced file was on screen. Stats files; never reads them.
    pub fn mark_seen(&mut self) {
        if self.seen_revision == self.revision && !self.accept_files {
            return;
        }
        let (listed, _) = workspace::list_closure(&self.folder, &self.fields, &self.body);
        let current: Vec<(String, FileStamp)> = listed
            .iter()
            .map(|file| (file.path.clone(), file.stamp))
            .collect();
        let accept = self.painted_files.is_none() || self.accept_files;
        self.accept_files = false;
        let mut fresh = Vec::new();
        if accept {
            fresh = current.iter().map(|(path, _)| path.clone()).collect();
            self.painted_files = Some(current.clone());
        } else if let Some(painted) = &mut self.painted_files {
            painted.retain(|(path, _)| current.iter().any(|(name, _)| name == path));
            for (path, stamp) in &current {
                if painted.iter().all(|(name, _)| name != path) {
                    painted.push((path.clone(), *stamp));
                    fresh.push(path.clone());
                }
            }
            painted.sort();
        }
        for path in &fresh {
            self.digests.forget(path);
        }
        let painted = self.painted_files.clone().unwrap_or_default();
        for (path, stamp) in &painted {
            if current.contains(&(path.clone(), *stamp)) {
                self.digests.request(&self.folder, path, *stamp);
            }
        }
        let mut keep = painted;
        keep.extend(current);
        self.digests.retain(&keep);
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

    /// The window showing this draft closed. Agents may delete it again.
    pub fn close_editor(&mut self) {
        self.open = false;
        self.server_save.close();
        self.focused = false;
    }

    /// Focus the editor. Does not transmit.
    pub fn request_send(&mut self) -> bool {
        self.open = true;
        self.focused = true;
        false
    }

    /// Remember which agent asked for the editor, so it can say so.
    pub fn note_send_request(&mut self, agent: &str) {
        self.send_requested_by = Some(agent.to_string());
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
        if self.send_lock.is_some() {
            return;
        }
        if body == self.body {
            return;
        }
        self.begin_user_change();
        let before = self.hunk_base();
        self.body = body;
        self.check_signature();
        self.bump();
        self.follow_user_change(before);
        self.refresh_issues();
    }

    pub fn set_subject_from_user(&mut self, subject: String) {
        if self.send_lock.is_some() {
            return;
        }
        if subject == self.fields.subject {
            return;
        }
        self.begin_user_change();
        let before = self.hunk_base();
        self.fields.subject = subject;
        self.bump();
        self.follow_user_change(before);
        self.refresh_issues();
    }

    pub fn identities(&self) -> &[Identity] {
        &self.identities
    }

    pub fn managed_signature(&self) -> Option<&str> {
        self.managed_signature.as_deref()
    }

    pub fn reply_context(&self) -> Option<&ReplyContext> {
        self.reply_context.as_ref()
    }

    pub fn set_identities(&mut self, identities: Vec<Identity>) -> bool {
        if self.identities == identities {
            return false;
        }
        self.identities = identities;
        true
    }

    fn identity(&self, from: &str) -> Option<&Identity> {
        self.identity_at_address(&email_of(from)?)
    }

    fn identity_at_address(&self, address: &str) -> Option<&Identity> {
        self.identities
            .iter()
            .find(|identity| identity.address.eq_ignore_ascii_case(address))
    }

    pub fn set_from_user(&mut self, address: &str) -> Result<bool, String> {
        self.ensure_editable()?;
        let picked = self
            .identity_at_address(address)
            .ok_or("from is not one of your addresses")?;
        let identity = if crate::is_demo_account(&self.account_id) {
            picked.clone()
        } else {
            crate::account_config::configured_sender(&self.account_id, address)?
        };
        let from = identity.from_header();
        let signature = self
            .managed_signature
            .as_ref()
            .map(|_| signature_block(&identity));
        if from == self.fields.from && signature == self.managed_signature {
            return Ok(false);
        }
        self.end_user_change();
        self.begin_user_change();
        self.end_user_change();
        let before = self.hunk_base();
        self.fields.from = from;
        if let Some(signature) = signature {
            self.swap_signature(signature);
        }
        self.bump();
        self.follow_user_change(before);
        self.refresh_issues();
        Ok(true)
    }

    fn check_signature(&mut self) {
        if let Some(block) = &self.managed_signature
            && !block.is_empty()
            && signature_range(&self.body, block).is_none()
        {
            self.managed_signature = None;
        }
    }

    fn swap_signature(&mut self, next: String) {
        let Some(block) = &self.managed_signature else {
            return;
        };
        if *block == next {
            return;
        }
        if block.is_empty() {
            append_signature(&mut self.body, &next);
        } else if let Some(range) = signature_range(&self.body, block) {
            self.body.replace_range(range, &next);
        } else {
            return;
        }
        self.managed_signature = Some(next);
    }

    pub fn has_only_signature(&self) -> bool {
        self.managed_signature.as_deref().is_some_and(|block| {
            !block.is_empty()
                && self
                    .body
                    .strip_suffix(block)
                    .is_some_and(|prefix| prefix.trim().is_empty())
        })
    }

    pub fn set_addresses_from_user(&mut self, field: &str, line: &str) -> Result<(), String> {
        self.ensure_editable()?;
        let parsed = parse_address_line(line)?;
        let current = match field {
            "to" => &self.fields.to,
            "cc" => &self.fields.cc,
            "bcc" => &self.fields.bcc,
            _ => return Err("unknown address field".into()),
        };
        if *current == parsed {
            return Ok(());
        }
        self.begin_user_change();
        let before = self.hunk_base();
        match field {
            "to" => self.fields.to = parsed,
            "cc" => self.fields.cc = parsed,
            _ => self.fields.bcc = parsed,
        }
        self.bump();
        self.follow_user_change(before);
        self.refresh_issues();
        Ok(())
    }

    pub fn undo(&mut self) -> bool {
        if self.send_lock.is_some() {
            return false;
        }
        self.undo_open = false;
        let Some(previous) = self.undo.pop() else {
            return false;
        };
        self.fields = previous.fields;
        self.body = previous.body;
        self.managed_signature = previous.managed_signature;
        self.hunks = previous.hunks;
        self.notices = previous.notices;
        self.bump();
        self.refresh_issues();
        true
    }

    pub fn keep_hunk(&mut self, id: u64) -> bool {
        if self.send_lock.is_some() {
            return false;
        }
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

    /// Put back the text an agent edit replaced, at that edit's range and
    /// nowhere else. Refuses, with a notice, when that cannot be done.
    pub fn reject_hunk(&mut self, id: u64) -> bool {
        if self.send_lock.is_some() {
            return false;
        }
        let Some(index) = self.hunks.iter().position(|hunk| hunk.id == id) else {
            return false;
        };
        let hunk = self.hunks[index].clone();
        let text = self.serialize();
        if !hunk.holds(&text) {
            self.notices
                .push("that agent edit is no longer where it was; nothing was reverted".into());
            return false;
        }
        let (next, change) = review::apply(&text, &(hunk.at..hunk.end()), &hunk.previous);
        let Ok(parsed) = parse_draft(&next) else {
            self.notices.push(
                "reverting that agent edit would break the draft header; nothing was reverted"
                    .into(),
            );
            return false;
        };
        self.begin_user_change();
        self.end_user_change();
        self.hunks.remove(index);
        review::absorb(&mut self.hunks, &text, &next, &change, None);
        let identity = if parsed.fields.from != self.fields.from {
            self.identity(&parsed.fields.from).cloned()
        } else {
            None
        };
        self.adopt_parsed(parsed, identity);
        let now = self.serialize();
        review::carry(&mut self.hunks, &next, &now);
        true
    }

    pub fn edit(&mut self, base_revision: u64, old: &str, new: &str) -> Result<(), String> {
        self.edit_impl(base_revision, old, new, false)
    }

    pub(crate) fn edit_as_agent(
        &mut self,
        base_revision: u64,
        old: &str,
        new: &str,
    ) -> Result<(), String> {
        self.edit_impl(base_revision, old, new, true)
    }

    fn edit_impl(
        &mut self,
        base_revision: u64,
        old: &str,
        new: &str,
        verify_config: bool,
    ) -> Result<(), String> {
        self.ensure_editable()?;
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
        let (next, change) = review::apply(&current, &(at..at + old.len()), new);
        let parsed = parse_draft(&next).map_err(|err| err.message)?;
        let identity = if parsed.fields.from == self.fields.from {
            None
        } else {
            let configured = if verify_config {
                crate::account_config::configured_sender(&self.account_id, &parsed.fields.from).ok()
            } else {
                self.identity(&parsed.fields.from).cloned()
            };
            Some(configured.ok_or("from is not one of your addresses")?)
        };
        self.undo.push(self.checkpoint());
        // The person's next keystroke starts a new undo step, so undo
        // takes back this edit and nothing typed after it.
        self.undo_open = false;
        if old != new {
            let kind = review::kind_of(&next, change.new.start, change.new.end);
            let id = self.take_hunk_id();
            review::absorb(&mut self.hunks, &current, &next, &change, Some((id, kind)));
        }
        self.adopt_parsed(parsed, identity);
        let now = self.serialize();
        review::carry(&mut self.hunks, &next, &now);
        self.settled = now;
        Ok(())
    }

    pub fn replace_body(&mut self, body: &str) -> Result<(), String> {
        self.ensure_editable()?;
        if body == self.body {
            return Ok(());
        }
        self.undo.push(self.checkpoint());
        self.undo_open = false;
        let before = self.serialize();
        self.body = body.to_string();
        self.check_signature();
        self.record_agent_rewrite(&before, Some(HunkKind::Body));
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

    /// What `draft.md` holds now. Call [`Self::push_disk_seen`] instead when
    /// the file was read before taking the session lock.
    pub fn push_disk(&mut self, content: Option<String>, now_ms: u64) {
        let since = self.disk_generation();
        self.push_disk_seen(content, since, now_ms);
    }

    /// The newest save on disk. A watcher samples this before reading
    /// `draft.md`, so a read that lands after a later save still knows the
    /// text it saw was Fell's own.
    pub fn disk_generation(&self) -> u64 {
        self.writer.as_ref().map_or(0, DraftWriter::written)
    }

    /// `content` was read after [`Self::disk_generation`] returned `since`.
    pub fn push_disk_seen(&mut self, content: Option<String>, since: u64, now_ms: u64) {
        match content {
            None => {
                self.pending = None;
                self.pending_delete = true;
                self.pending_since = now_ms;
            }
            Some(text) if self.is_own_write(&text, since) => {
                self.pending = None;
                self.pending_delete = false;
            }
            Some(text) => {
                self.pending = Some(text);
                self.pending_seen = since;
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
        if self.is_own_write(&text, self.pending_seen) {
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

    /// Queue the save an editor change is still waiting on, if any. Closing
    /// the editor or quitting calls this, so the debounce never drops typing.
    pub fn flush_pending(&mut self) -> Result<(), String> {
        if self.outbound_since.is_some() {
            self.flush()
        } else {
            Ok(())
        }
    }

    /// Save the draft file and its mailbox row, with the hunks.
    ///
    /// With a writer attached this only queues the save, so the caller's
    /// lock is not held across disk I/O; a failed write shows as a notice on
    /// the next save. Without one it writes before returning.
    pub fn flush(&mut self) -> Result<(), String> {
        self.flush_with_server(None)
    }

    pub(super) fn flush_server_if_due(&mut self, now_ms: Option<u64>, wake: &super::server::Wake) {
        if self.send_lock.is_none() && self.server_save.due(self.revision, now_ms) {
            let _ = self.flush_with_server(Some(wake.clone()));
        }
    }

    fn flush_with_server(&mut self, server: Option<super::server::Wake>) -> Result<(), String> {
        self.outbound_since = None;
        let text = self.serialize();
        let job = SaveJob {
            revision: self.revision,
            folder: self.folder.clone(),
            text: text.clone(),
            mailbox: Some(self.mailbox_save(&text)),
            server,
        };
        self.last_written = text.clone();
        if let Some(writer) = &self.writer {
            if let Some(err) = writer.take_error() {
                self.notices.push(format!("could not save draft.md: {err}"));
            }
            if let Some(err) = writer.take_mailbox_error() {
                note_mailbox_error(&mut self.notices, &err);
            }
            let number = writer.submit(job);
            self.own_writes.note(number, text);
            return Ok(());
        }
        match job.run(&mut self.save_cache) {
            Ok(_) => Ok(()),
            Err(SaveError::File(err)) => Err(err),
            Err(SaveError::Mailbox(err)) => {
                note_mailbox_error(&mut self.notices, &err);
                Ok(())
            }
        }
    }

    /// Run later saves on a thread of their own. The app attaches one for
    /// every draft it shows; tests and the file fallback save inline.
    pub fn attach_writer(&mut self) {
        if self.writer.is_none() {
            self.writer = Some(DraftWriter::spawn());
        }
    }

    pub fn save_receipt(&self) -> Option<super::SaveReceipt> {
        self.writer.as_ref()?.receipt(self.revision)
    }

    /// Waits for every save queued so far. Call it after releasing the
    /// session lock; `None` when saves are synchronous.
    pub fn save_waiter(&self) -> Option<SaveWaiter> {
        self.writer.as_ref().map(DraftWriter::waiter)
    }

    /// Save the state undo must restore, including typing still waiting on
    /// the editor's debounce. Any subsequent edit changes the generation.
    pub(crate) fn save_for_discard(&mut self) -> Result<Option<SaveWaiter>, String> {
        self.flush()?;
        Ok(self.save_waiter())
    }

    /// `text` is Fell's own: the last text it queued, or, with a writer, a
    /// save numbered `since` or later (a read made when save `since` was the
    /// newest on disk shows that save or a newer one).
    fn is_own_write(&self, text: &str, since: u64) -> bool {
        if text == self.last_written {
            return true;
        }
        self.writer.is_some() && self.own_writes.contains(since, text)
    }

    fn mailbox_save(&self, text: &str) -> MailboxSave {
        MailboxSave {
            store_path: self.store_path.clone(),
            draft: persist::StoredCompose {
                id: self.id.clone(),
                account_id: self.account_id.clone(),
                fields: self.fields.clone(),
                body: self.body.clone(),
                files: Vec::new(),
                identities: self
                    .identities
                    .iter()
                    .map(|identity| identity.address.clone())
                    .collect(),
                managed_signature: self.managed_signature.clone(),
                owner: self.owner.clone(),
                reply: self.reply.clone(),
            },
            review: StoredReview {
                text: text.to_string(),
                hunks: review::to_json(&self.hunks),
                next_hunk: self.next_hunk,
            },
        }
    }

    pub fn closure(&self) -> Closure {
        let mut closure = workspace::resolve_closure(&self.folder, &self.fields, &self.body);
        closure.issues.extend(self.issues.clone());
        closure
    }

    /// Check the draft and build the message. Reads the attachments once;
    /// those bytes are compared with what was on screen and are the bytes
    /// sent. [`Self::resolved_files`] hands the same bytes back.
    pub fn prepare_send(&self) -> Result<Outgoing, String> {
        if let Some(reason) = self.send_block() {
            return Err(reason);
        }
        let closure = workspace::resolve_closure(&self.folder, &self.fields, &self.body);
        if let Some(issue) = closure.issues.iter().find(|issue| issue.blocks_send) {
            return Err(issue.message.clone());
        }
        self.check_sent_files(&closure)?;
        let outgoing = mime::build_outgoing(
            self.account_id.clone(),
            &self.fields,
            &self.body,
            &closure.files,
            &self.reply,
        );
        *self.prepared.lock().expect("prepared send") = Some((self.revision, closure.files));
        Ok(outgoing)
    }

    /// Each file about to be sent is the version the person saw. Its bytes
    /// are hashed here and must match that version's digest; a file that
    /// was not hashed yet must still be that exact version.
    fn check_sent_files(&self, closure: &Closure) -> Result<(), String> {
        let painted = self.painted_files.as_deref().unwrap_or(&[]);
        for file in &closure.files {
            let stamp = closure
                .stamps
                .iter()
                .find(|(path, _)| path == &file.path)
                .map(|(_, stamp)| *stamp);
            let shown = painted
                .iter()
                .find(|(path, _)| path == &file.path)
                .map(|(_, stamp)| *stamp);
            let (Some(stamp), Some(shown)) = (stamp, shown) else {
                return Err(CHANGED_ATTACHMENT.into());
            };
            let digest = digest::sha256(&file.bytes);
            match self.digests.get(&file.path, shown) {
                Some(Known::Digest(seen)) if seen == digest => {}
                // The shown version is not hashed yet. Sending now could
                // carry bytes that changed within the same timestamp tick.
                None => {
                    self.digests.request(&self.folder, &file.path, shown);
                    return Err(CHECKING_ATTACHMENT.into());
                }
                _ => return Err(CHANGED_ATTACHMENT.into()),
            }
            self.digests.record(&file.path, stamp, digest);
            if self.digests.get(&file.path, stamp) == Some(Known::Conflict) {
                return Err(CHANGED_ATTACHMENT.into());
            }
        }
        Ok(())
    }

    pub fn snapshot(&self) -> Snapshot {
        let text = self.serialize();
        let (listed, listed_issues) =
            workspace::list_closure(&self.folder, &self.fields, &self.body);
        let send_block = self.send_block_listed(&listed, &listed_issues);
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
            quoted: Some(self.reply.quote_html.clone()).filter(|html| !html.trim().is_empty()),
            open: self.open,
            focused: self.focused,
            send_block,
            text,
            send_requested_by: self.send_requested_by.clone(),
            reply_context: self.reply_context.clone(),
            reply_to_unverified: self.reply.reply_to_unverified,
            forwarded: super::reply::forward_summary(&self.reply.quote_text),
            agent_authored: !self.owner.is_empty(),
        }
    }

    pub fn fields(&self) -> &DraftFields {
        &self.fields
    }

    pub fn body(&self) -> &str {
        &self.body
    }

    /// The attachments to submit. Right after [`Self::prepare_send`] these
    /// are the bytes it checked and put in the message, not a second read.
    pub fn resolved_files(&self) -> Vec<ResolvedFile> {
        let prepared = self.prepared.lock().expect("prepared send").take();
        if let Some((revision, files)) = prepared
            && revision == self.revision
        {
            return files;
        }
        workspace::resolve_closure(&self.folder, &self.fields, &self.body).files
    }

    /// Where to save `outgoing` once SMTP confirms delivery.
    pub fn sent_copy(&self, outgoing: &Outgoing) -> Option<SentCopy> {
        let store = self
            .store_path
            .clone()
            .or_else(|| persist::remembered_store_path(&self.id))?;
        let address = |raw: &String| {
            super::frontmatter::mailbox_parts(raw)
                .map(|(name, email)| crate::Address { name, email })
        };
        Some(SentCopy {
            store,
            draft_id: self.id.clone(),
            account_id: outgoing.draft.account_id.clone(),
            message_id: outgoing.message_id.clone(),
            bcc: outgoing.draft.bcc.iter().filter_map(address).collect(),
            visible: outgoing
                .draft
                .to
                .iter()
                .chain(&outgoing.draft.cc)
                .filter_map(address)
                .collect(),
        })
    }

    pub fn send_lock(&self) -> Option<SendLock> {
        self.send_lock
    }

    /// Once a send starts the draft is read-only: what goes out, and what
    /// the editor then shows as sent, is the text the checks ran on. It opens
    /// again only if the send failed before the server had the message.
    fn ensure_editable(&self) -> Result<(), String> {
        match self.send_lock {
            Some(lock) => Err(format!("{}; it cannot be edited", lock.reason())),
            None => Ok(()),
        }
    }

    /// Claim this draft for one send. Fails while another send is running,
    /// after one went, or may have gone, out, and when the claim cannot be
    /// recorded in the mailbox.
    ///
    /// The mailbox records the claim as "may have been sent" before anything
    /// is transmitted: if Fell stops mid-send, the next start must not send
    /// the draft again without the person choosing to unlock it. A send
    /// that cannot leave that record does not start.
    pub fn begin_send(&mut self, message_id: &crate::MessageId) -> Result<(), String> {
        if let Some(lock) = self.send_lock {
            return Err(lock.reason().into());
        }
        let claimed = self.send_store().and_then(|store| {
            let guard = store
                .lock_compose_send(&self.id)
                .map_err(|err| err.to_string())?;
            let lock = store
                .claim_compose_send(&self.account_id, &self.id, Some(message_id.as_str()))
                .map_err(|err| err.to_string())?;
            Ok((lock, guard))
        });
        match claimed {
            Ok((None, guard)) => {
                self.send_lock = Some(SendLock::InFlight);
                self.send_guard = guard.map(std::sync::Arc::new);
                Ok(())
            }
            // The mailbox knows of a send this session did not see.
            Ok((Some(lock), _)) => {
                self.send_lock = Some(lock);
                Err(lock.reason().into())
            }
            Err(err) => Err(format!("could not record this send in the mailbox ({err})")),
        }
    }

    /// The person accepted the risk of a duplicate: make an uncertain draft
    /// editable and sendable again. Only that lock is released, in the
    /// mailbox first; a recorded send never is.
    pub fn release_maybe_sent(&mut self) -> Result<(), String> {
        if self.send_lock != Some(SendLock::MaybeSent) {
            return Err(match self.send_lock {
                Some(lock) => format!("{}; it cannot be unlocked", lock.reason()),
                None => "this draft is not locked".into(),
            });
        }
        let store = self.send_store()?;
        let _guard = store
            .lock_compose_send(&self.id)
            .map_err(|err| err.to_string())?;
        if store
            .compose_is_discarded(&self.id)
            .map_err(|err| err.to_string())?
        {
            return Err("Draft was discarded".into());
        }
        store
            .finish_compose_send(&self.account_id, &self.id, None)
            .map_err(|err| format!("could not unlock this draft in the mailbox ({err})"))?;
        // Another process may have recorded the send meanwhile.
        match store
            .load_compose_send(&self.account_id, &self.id)
            .map_err(|err| err.to_string())?
        {
            None => {
                self.send_lock = None;
                Ok(())
            }
            Some(lock) => {
                self.send_lock = Some(lock);
                Err(format!("{}; it cannot be unlocked", lock.reason()))
            }
        }
    }

    /// Record how [`DraftSession::begin_send`]'s send ended. `None` means
    /// nothing was transmitted and the draft may be sent again.
    pub fn end_send(&mut self, lock: Option<SendLock>) {
        debug_assert_ne!(lock, Some(SendLock::InFlight));
        self.send_lock = lock;
        let recorded = self.send_store().and_then(|store| {
            if lock != Some(SendLock::MaybeSent) {
                store
                    .finish_compose_send(&self.account_id, &self.id, lock)
                    .map_err(|err| err.to_string())?;
            }
            // Sync may have found the copy while SMTP was still waiting.
            // An in-flight editor is left alone until its transfer ends.
            if lock != Some(SendLock::Sent) && self.has_send_confirmation(&store) {
                self.send_lock = Some(SendLock::Sent);
            }
            Ok(())
        });
        if let Err(err) = recorded
            && lock != Some(SendLock::MaybeSent)
        {
            // The claim stays in the mailbox, which errs toward "may have
            // been sent".
            self.notices.push(format!(
                "could not record how this send ended ({err}); after a restart this draft will read as possibly sent"
            ));
        }
        self.send_guard = None;
    }

    /// Tell the editor that delivery succeeded but its local copy could not be saved.
    pub fn report_sent_copy_failure(&mut self) {
        self.notices.push(super::LOCAL_SENT_COPY_FAILED.into());
    }

    /// Apply confirmation of a recorded uncertain send. A live transfer
    /// still owns its editor until the SMTP worker ends it.
    pub(crate) fn confirm_uncertain_send(&mut self) {
        if self.send_lock == Some(SendLock::MaybeSent) {
            self.send_lock = Some(SendLock::Sent);
        }
    }

    /// Catch a confirmation that committed while this session was opening.
    pub fn refresh_send_confirmation(&mut self, store: &crate::Store) {
        if self.send_lock == Some(SendLock::MaybeSent) && self.has_send_confirmation(store) {
            self.confirm_uncertain_send();
        }
    }

    fn has_send_confirmation(&self, store: &crate::Store) -> bool {
        matches!(
            store.load_compose_send(&self.account_id, &self.id),
            Ok(Some(SendLock::Sent))
        )
    }

    /// The mailbox this draft's send record lives in. Opened for one write,
    /// like a save. Never creates a mailbox.
    fn send_store(&self) -> Result<crate::Store, String> {
        let path = self
            .mailbox_path()
            .filter(|path| path.is_file())
            .ok_or_else(|| String::from("this draft has no mailbox"))?;
        crate::Store::open(&path).map_err(|err| err.to_string())
    }

    pub(crate) fn mailbox_path(&self) -> Option<PathBuf> {
        self.store_path
            .clone()
            .or_else(|| persist::remembered_store_path(&self.id))
    }

    /// Keep the lock of the session this one replaces in the hub.
    pub(crate) fn inherit_send_lock(&mut self, previous: &DraftSession) {
        // Keep a live transfer, and never replace a recorded confirmation
        // with an older uncertain lock.
        if previous.send_lock == Some(SendLock::InFlight)
            || (self.send_lock != Some(SendLock::Sent) && previous.send_lock.is_some())
        {
            self.send_lock = previous.send_lock;
            self.send_guard.clone_from(&previous.send_guard);
        }
    }

    fn send_block(&self) -> Option<String> {
        let (listed, issues) = workspace::list_closure(&self.folder, &self.fields, &self.body);
        self.send_block_listed(&listed, &issues)
    }

    /// Runs on every paint. Stats files; never reads them.
    fn send_block_listed(
        &self,
        listed: &[ListedFile],
        listed_issues: &[DraftIssue],
    ) -> Option<String> {
        if let Some(lock) = self.send_lock {
            return Some(lock.reason().into());
        }
        if self.seen_revision != self.revision {
            return Some("the draft changed; it is on screen before send".into());
        }
        if let Some(painted) = &self.painted_files
            && self.files_changed(painted, listed)
        {
            return Some(CHANGED_ATTACHMENT.into());
        }
        if let Some(issue) = self.issues.iter().find(|issue| issue.blocks_send) {
            return Some(issue.message.clone());
        }
        if self.fields.to.is_empty() {
            return Some("add at least one recipient".into());
        }
        if self.identity(&self.fields.from).is_none() {
            return Some("from is not one of your addresses".into());
        }
        listed_issues
            .iter()
            .find(|issue| issue.blocks_send)
            .map(|issue| issue.message.clone())
    }

    /// A referenced file is not the version that was on screen. A new
    /// version whose digest matches the shown one (a `touch`, an identical
    /// copy) is not a change once the hash thread has read it.
    fn files_changed(&self, painted: &[(String, FileStamp)], listed: &[ListedFile]) -> bool {
        let mut changed = painted
            .iter()
            .any(|(path, _)| listed.iter().all(|file| &file.path != path));
        for file in listed {
            let Some((_, shown)) = painted.iter().find(|(path, _)| path == &file.path) else {
                changed = true;
                continue;
            };
            if file.stamp == *shown {
                // Same stamp, but the hash thread saw two different contents.
                if self.digests.get(&file.path, *shown) == Some(Known::Conflict) {
                    changed = true;
                }
                continue;
            }
            match (
                self.digests.get(&file.path, *shown),
                self.digests.get(&file.path, file.stamp),
            ) {
                (Some(Known::Digest(before)), Some(Known::Digest(now))) if before == now => {}
                (_, None) => {
                    self.digests.request(&self.folder, &file.path, file.stamp);
                    changed = true;
                }
                _ => changed = true,
            }
        }
        changed
    }

    fn merge_disk(&mut self, text: &str) {
        if let Some(lock) = self.send_lock {
            // What was sent (or is being sent) stays what is on disk.
            self.notices.push(format!(
                "draft.md changed, but {}; kept the sent text",
                lock.reason()
            ));
            let _ = self.flush();
            return;
        }
        if parse_draft(text).is_err() {
            self.notices.push(
                "draft.md could not be parsed; kept the open draft and rewrote the file".into(),
            );
            let _ = self.flush();
            return;
        }
        let ours = self.serialize();
        let outcome = merge::merge_draft(&self.settled, &ours, text);
        let changed = outcome.text != merge::normalize(&ours);
        let checkpoint = self.checkpoint();
        if !self.apply_agent_merge(&ours, outcome) {
            self.notices.push(
                "draft.md could not be parsed; kept the open draft and rewrote the file".into(),
            );
            let _ = self.flush();
            return;
        }
        if changed {
            // Undo takes back this file edit, not the person's last change.
            self.undo.push(checkpoint);
            self.undo_open = false;
        }
        self.settled = self.serialize();
        let _ = self.flush();
    }

    /// Adopt a merge whose hunk ranges point into `ours`, the text the
    /// current hunks point into. Each merged region becomes an agent hunk.
    fn apply_agent_merge(&mut self, ours: &str, outcome: MergeOutcome) -> bool {
        let Ok(parsed) = parse_draft(&outcome.text) else {
            return false;
        };
        self.notices.extend(outcome.skipped);
        let mut text = merge::normalize(ours);
        review::carry(&mut self.hunks, ours, &text);
        let mut regions = outcome.hunks;
        regions.sort_by_key(|region| Reverse(region.old_start));
        for region in regions {
            let range = region.old_start..region.old_end;
            if text.get(range.clone()) != Some(region.previous.as_str()) {
                continue;
            }
            let (next, change) = review::apply(&text, &range, &region.inserted);
            let id = self.take_hunk_id();
            review::absorb(
                &mut self.hunks,
                &text,
                &next,
                &change,
                Some((id, region.kind)),
            );
            text = next;
        }
        review::carry(&mut self.hunks, &text, &outcome.text);
        let identity = (parsed.fields.from != self.fields.from)
            .then(|| {
                crate::account_config::configured_sender(&self.account_id, &parsed.fields.from).ok()
            })
            .flatten();
        self.adopt_parsed(parsed, identity);
        let now = self.serialize();
        review::carry(&mut self.hunks, &outcome.text, &now);
        true
    }

    /// One agent hunk for everything that differs between `before` and now.
    fn record_agent_rewrite(&mut self, before: &str, kind: Option<HunkKind>) {
        let now = self.serialize();
        let edits = review::diff(before, &now);
        let (Some(first), Some(last)) = (edits.first(), edits.last()) else {
            return;
        };
        let range = first.old.start..last.old.end;
        let (next, change) = review::apply(before, &range, &now[first.new.start..last.new.end]);
        debug_assert_eq!(next, now);
        let kind = kind.unwrap_or_else(|| review::kind_of(&next, change.new.start, change.new.end));
        let id = self.take_hunk_id();
        review::absorb(&mut self.hunks, before, &next, &change, Some((id, kind)));
    }

    fn adopt_parsed(&mut self, parsed: ParsedDraft, identity: Option<Identity>) {
        let mut fields = parsed.fields;
        let changed_address = email_of(&fields.from) != email_of(&self.fields.from);
        if let Some(identity) = &identity {
            fields.from = identity.from_header();
        } else if fields.from != self.fields.from {
            fields.from.clone_from(&self.fields.from);
            self.notices
                .push("from is not one of your addresses; the edit was ignored".into());
        }
        if fields.in_reply_to != self.locked_reply {
            fields.in_reply_to = self.locked_reply.clone();
            self.notices
                .push("in_reply_to is set by Fell; the edit was ignored".into());
        }
        self.fields = fields;
        self.body = parsed.body;
        self.check_signature();
        if let Some(identity) = identity.filter(|_| changed_address) {
            self.swap_signature(signature_block(&identity));
        }
        self.issues = parsed.issues;
        self.bump();
    }

    fn refresh_issues(&mut self) {
        if let Ok(parsed) = parse_draft(&self.serialize()) {
            self.issues = parsed.issues;
        }
    }

    fn take_hunk_id(&mut self) -> u64 {
        let id = self.next_hunk;
        self.next_hunk += 1;
        id
    }

    /// The text hunk ranges point into, when there are hunks to carry.
    fn hunk_base(&self) -> Option<String> {
        (!self.hunks.is_empty()).then(|| self.serialize())
    }

    /// The person changed the draft since `before`. Hunks move with the
    /// text; one whose text the person rewrote is theirs now.
    fn follow_user_change(&mut self, before: Option<String>) {
        let Some(before) = before else {
            return;
        };
        let after = self.serialize();
        if review::follow_user(&mut self.hunks, &before, &after) > 0 {
            self.notices
                .push("kept your text where it overlapped an agent edit".into());
        }
    }

    /// The draft with every unreviewed agent edit taken back.
    fn user_serialized(&self) -> String {
        let mut text = self.serialize();
        let mut hunks: Vec<&Hunk> = self.hunks.iter().collect();
        hunks.sort_by_key(|hunk| Reverse(hunk.at));
        for hunk in hunks {
            if hunk.holds(&text) {
                text.replace_range(hunk.at..hunk.end(), &hunk.previous);
            }
        }
        text
    }

    /// Classify the same recipient while it is being edited and after it is saved.
    pub fn address_view(&self, raw: &str) -> AddressView {
        let email = email_of(raw).unwrap_or_default();
        let fresh = !email.is_empty() && !self.known.contains(&email);
        AddressView {
            raw: raw.to_owned(),
            email,
            fresh,
        }
    }

    fn address_views(&self, addresses: &[String]) -> Vec<AddressView> {
        addresses.iter().map(|raw| self.address_view(raw)).collect()
    }

    fn checkpoint(&self) -> Checkpoint {
        Checkpoint {
            fields: self.fields.clone(),
            body: self.body.clone(),
            managed_signature: self.managed_signature.clone(),
            hunks: self.hunks.clone(),
            notices: self.notices.clone(),
        }
    }

    fn bump(&mut self) {
        self.revision = self.revision.saturating_add(1);
    }
}

fn address_only(address: String) -> Identity {
    Identity {
        address,
        name: String::new(),
        signature: None,
    }
}

/// The exact generated text must still be the final lines of the body.
pub(crate) fn signature_range(body: &str, block: &str) -> Option<std::ops::Range<usize>> {
    let start = body.strip_suffix(block)?.len();
    (start == 0 || body[..start].ends_with('\n')).then_some(start..body.len())
}

pub fn signature_block(identity: &Identity) -> String {
    identity
        .signature
        .as_deref()
        .filter(|signature| !signature.trim().is_empty())
        .map(|signature| format!("-- \n{}\n", signature.trim_end_matches('\n')))
        .unwrap_or_default()
}

/// Place a generated signature after the draft's text with a blank line between them.
pub fn append_signature(body: &mut String, signature: &str) {
    if signature.is_empty() {
        return;
    }
    if !body.ends_with('\n') {
        body.push('\n');
    }
    if !body.ends_with("\n\n") {
        body.push('\n');
    }
    body.push_str(signature);
}

/// A review row belongs to the compose row it was saved with. One left by
/// another draft with this id, or by a save that did not reach the row, is
/// not used.
/// `draft.md` holds a save, but the mailbox row, Fell's own copy, does not.
/// Each distinct failure is said once.
fn note_mailbox_error(notices: &mut Vec<String>, err: &str) {
    let notice = format!("could not save this draft to the mailbox: {err}");
    if !notices.contains(&notice) {
        notices.push(notice);
    }
}

fn review_matches_row(text: &str, stored: &persist::StoredCompose) -> bool {
    let Ok(parsed) = parse_draft(text) else {
        return false;
    };
    parsed.body == stored.body
        && parsed.fields.from == stored.fields.from
        && parsed.fields.to == stored.fields.to
        && parsed.fields.cc == stored.fields.cc
        && parsed.fields.bcc == stored.fields.bcc
        && parsed.fields.subject == stored.fields.subject
}

/// Where builds before the mailbox record kept a draft's send state, in
/// the agent-writable control folder. Read once, on open, to migrate it.
const LEGACY_SEND_STATE: &str = "send-state";

fn read_legacy_send_state(path: &Path) -> Option<SendLock> {
    if std::fs::symlink_metadata(path).is_err() {
        return None;
    }
    // Any record other than a clean "sent" (unreadable, empty, or partial)
    // errs toward "may have been sent".
    match std::fs::read_to_string(path).ok().as_deref().map(str::trim) {
        Some("sent") => Some(SendLock::Sent),
        _ => Some(SendLock::MaybeSent),
    }
}

/// Path and version of each referenced file. Stats only.
pub(crate) fn attachment_stamp(
    folder: &Path,
    fields: &DraftFields,
    body: &str,
) -> Vec<(String, FileStamp)> {
    let (files, _) = workspace::list_closure(folder, fields, body);
    let mut stamp: Vec<(String, FileStamp)> = files
        .into_iter()
        .map(|file| (file.path, file.stamp))
        .collect();
    stamp.sort();
    stamp
}

fn hunk_view(hunk: &Hunk, text: &str) -> Option<HunkView> {
    if hunk.inserted.is_empty() || !hunk.holds(text) {
        return None;
    }
    let label = match hunk.kind {
        HunkKind::Recipients => "agent changed recipients",
        HunkKind::Subject => "agent changed the subject",
        HunkKind::Attachments => "agent changed attachments",
        HunkKind::Body => "agent edit",
    };
    Some(HunkView {
        id: hunk.id,
        kind: hunk.kind,
        start: hunk.at,
        end: hunk.end(),
        label: label.to_string(),
        previous: hunk.previous.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::super::frontmatter::AttachmentRef;
    use super::*;
    use std::fs;

    fn temp_session(body: &str) -> (tempfile::TempDir, DraftSession) {
        let temp = tempfile::tempdir().unwrap();
        let fields = DraftFields {
            from: "jon@example.com".into(),
            to: vec!["ada@example.com".into()],
            subject: "Hello".into(),
            in_reply_to: Some("<abc@example.com>".into()),
            ..DraftFields::default()
        };
        let folder =
            workspace::materialize_in(temp.path(), "d1", &fields, body, &AccountId("local".into()))
                .unwrap();
        let session = DraftSession::testing_create_local(
            folder,
            "d1",
            AccountId("local".into()),
            fields,
            body,
        )
        .unwrap();
        (temp, session)
    }

    #[test]
    fn managed_signature_swaps_and_undoes_with_from_but_keeps_edits_after_reopen() {
        let first = Identity {
            address: "jon@example.com".into(),
            name: "Jon".into(),
            signature: Some("**Primary**\nTeam".into()),
        };
        let second = Identity {
            address: "alias@example.com".into(),
            name: "Studio".into(),
            signature: Some("**Studio**\nSupport".into()),
        };
        let unsigned = Identity {
            address: "plain@example.com".into(),
            name: String::new(),
            signature: None,
        };
        let first_signature = signature_block(&first);
        let body = format!("Letter\n\n{first_signature}");
        let (temp, mut session) = temp_session(&body);
        session.set_identities(vec![first.clone(), second.clone(), unsigned.clone()]);
        session.managed_signature = Some(first_signature.clone());
        session.set_from_user(&second.address).unwrap();
        assert_eq!(session.fields().from, "Studio <alias@example.com>");
        assert_eq!(session.body(), "Letter\n\n-- \n**Studio**\nSupport\n");
        assert!(session.undo());
        assert_eq!(session.body(), body);
        session.set_from_user(&unsigned.address).unwrap();
        assert_eq!(session.body(), "Letter\n\n");
        session.set_from_user(&first.address).unwrap();
        assert!(session.body().ends_with(&first_signature));

        let db = temp.path().join("mail.db");
        let store = crate::Store::open(&db).unwrap();
        store
            .upsert_account(&crate::Account {
                id: session.account_id().clone(),
                email: first.address.clone(),
                display_name: first.name.clone(),
                backend_kind: crate::BackendKind::Bridge,
            })
            .unwrap();
        let account = session.account_id().clone();
        let folder = session.folder().to_path_buf();
        session.bind_store(&db);
        session.flush().unwrap();
        let mut reopened = DraftSession::open_with_store(&folder, account.clone(), &store).unwrap();
        reopened.set_identities(vec![first.clone(), second.clone()]);
        reopened.set_from_user(&second.address).unwrap();
        assert!(reopened.body().ends_with(&signature_block(&second)));

        reopened.set_body_from_user(reopened.body().replace("Support", "My edit"));
        reopened.flush().unwrap();
        let mut reopened = DraftSession::open_with_store(&folder, account, &store).unwrap();
        reopened.set_identities(vec![first.clone(), second]);
        reopened.set_from_user(&first.address).unwrap();
        assert!(reopened.body().ends_with("-- \n**Studio**\nMy edit\n"));
    }

    #[test]
    fn picking_an_address_uses_its_latest_name_and_signature() {
        if crate::test_support::in_isolated_process().is_none() {
            return;
        }
        let (_temp, mut session) = temp_session("Letter\n\n");
        session.managed_signature = Some(String::new());
        let mut alias = address_only("alias@example.com".into());
        session.set_identities(vec![alias.clone()]);
        let picked = session.identities()[0].address.to_uppercase();

        alias.name = "Studio".into();
        alias.signature = Some("Studio team".into());
        session.set_identities(vec![alias.clone()]);
        assert!(session.set_from_user(&picked).unwrap());
        assert_eq!(session.fields().from, "Studio <alias@example.com>");
        assert_eq!(session.body(), "Letter\n\n-- \nStudio team\n");

        alias.signature = Some("New studio footer".into());
        session.set_identities(vec![alias]);
        assert!(session.set_from_user(&picked).unwrap());
        assert_eq!(session.body(), "Letter\n\n-- \nNew studio footer\n");
        assert!(!session.set_from_user(&picked).unwrap());

        session.set_identities(vec![address_only("jon@example.com".into())]);
        assert!(session.set_from_user(&picked).is_err());
    }

    #[test]
    fn agent_cannot_set_an_unconfigured_from_address() {
        let Some(isolated) = crate::test_support::in_isolated_process() else {
            return;
        };
        let (temp, mut session) = temp_session("Letter\n");
        // The usual test session uses a reserved demo id.
        session.account_id = AccountId("mail".into());
        let old = "from: jon@example.com";
        let forged = "from: other@example.com";
        assert!(
            session
                .edit_as_agent(session.revision(), old, forged)
                .is_err()
        );
        assert_eq!(session.fields().from, "jon@example.com");
        let changed = session
            .serialize()
            .replacen(old, forged, 1)
            .replace("Letter", "Body edit");
        session.merge_disk(&changed);
        assert_eq!(session.fields().from, "jon@example.com");
        assert_eq!(session.body(), "Body edit\n");

        let config = isolated.join("config/fell/accounts");
        fs::create_dir_all(&config).unwrap();
        let path = config.join("mail.toml");
        fs::write(&path, format!(
            "id = {:?}\nemail = \"jon@example.com\"\npassword = \"test\"\n[[identities]]\naddress = \"other@example.com\"\nname = \"Studio\"\n",
            session.account_id().as_str()
        )).unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let forged = r#"from: '"=?UTF-8?Q?PayPal?=" <other@example.com>'"#;
        let canonical = r#"from: "Studio <other@example.com>""#;
        session
            .edit_as_agent(session.revision(), old, forged)
            .unwrap();
        assert_eq!(session.fields().from, "Studio <other@example.com>");
        session.merge_disk(&session.serialize().replacen(canonical, forged, 1));
        assert_eq!(session.fields().from, "Studio <other@example.com>");
        session.merge_disk(&session.serialize().replacen(canonical, old, 1));
        assert_eq!(session.fields().from, "jon@example.com");

        let db = temp.path().join("mail.db");
        let store = crate::Store::open(&db).unwrap();
        store
            .upsert_account(&crate::Account {
                id: session.account_id().clone(),
                email: "jon@example.com".into(),
                display_name: String::new(),
                backend_kind: crate::BackendKind::Bridge,
            })
            .unwrap();
        session.bind_store(&db);
        session.flush().unwrap();
        fs::write(
            session.folder().join("draft.md"),
            session.serialize().replacen(old, forged, 1),
        )
        .unwrap();
        let reopened =
            DraftSession::open_with_store(session.folder(), session.account_id().clone(), &store)
                .unwrap();
        assert_eq!(reopened.fields().from, "Studio <other@example.com>");
    }

    #[test]
    fn a_save_the_mailbox_refused_is_said_as_a_notice() {
        let (temp, mut session) = temp_session("Hello\n");
        // A mailbox file that is not a mailbox: opening it fails.
        let broken = temp.path().join("broken.db");
        fs::write(&broken, "not sqlite, and longer than a header would be").unwrap();
        session.bind_store(&broken);
        session.set_body_from_user("typed\n".into());
        session.flush().unwrap();
        assert!(
            fs::read_to_string(session.folder().join("draft.md"))
                .unwrap()
                .contains("typed")
        );
        let notices = session.snapshot().notices;
        let said: Vec<_> = notices
            .iter()
            .filter(|notice| notice.starts_with("could not save this draft to the mailbox"))
            .collect();
        assert_eq!(said.len(), 1, "{notices:?}");
        // Through the writer thread too, reported on the next save.
        session.attach_writer();
        session.flush().unwrap();
        session.save_waiter().unwrap().wait().unwrap();
        session.flush().unwrap();
        let notices = session.snapshot().notices;
        assert!(
            notices
                .iter()
                .any(|notice| notice.starts_with("could not save this draft to the mailbox")),
            "{notices:?}"
        );
        // draft.md holds Fell's own save, so a cold open keeps it.
        let text = fs::read_to_string(session.folder().join("draft.md")).unwrap();
        assert!(workspace::is_own_save(session.folder(), &text));
        assert!(!workspace::is_own_save(session.folder(), "something else"));
    }

    #[test]
    fn request_send_does_not_produce_a_message() {
        let (_temp, mut session) = temp_session("Hello\n");
        let transmitted = session.request_send();
        assert!(!transmitted);
        assert!(session.is_open());
        assert!(session.try_agent_delete().is_err());
        session.mark_seen();
        session.digests.wait_idle();
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
        let (_temp, mut session) = temp_session("alpha\n");
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
        let (_temp, mut session) = temp_session("Hello paragraph\n");
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
        let (_temp, mut session) = temp_session("alpha\n");
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
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
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
                reply_to: Some(Vec::new()),
                attachment_names: Vec::new(),
                parts: None,
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
        let mut session = DraftSession::testing_create_local(
            folder.clone(),
            "d1",
            account.id.clone(),
            fields,
            "body\n",
        )
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
        let (temp, mut session) = temp_session("see ![chart](chart.png)\n");
        let db = temp.path().join("mail.db");
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
        let (_temp, mut session) = temp_session("Please send the report\n");
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
        let (_temp, mut session) = temp_session("Please send the report\n\nThanks\n");
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
        let (_temp, mut session) = temp_session("body\n");
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
        let (_temp, mut session) = temp_session("keep this body\n");
        session.push_disk(None, 0);
        assert!(session.poll_disk(DEBOUNCE_MS));
        assert_eq!(session.body(), "keep this body\n");
        let text = workspace::read_draft_file(session.folder()).unwrap();
        assert!(text.contains("keep this body"));
    }

    #[test]
    fn fell_ignores_its_own_write() {
        let (_temp, mut session) = temp_session("same\n");
        let written = session.serialize();
        let revision = session.revision();
        session.push_disk(Some(written), 0);
        assert!(!session.poll_disk(DEBOUNCE_MS));
        assert_eq!(session.revision(), revision);
    }

    #[test]
    fn yaml_errors_keep_the_body_and_block_send() {
        let (_temp, mut session) = temp_session("precious\n");
        session.push_disk(Some("this is not a draft\n".into()), 0);
        assert!(session.poll_disk(DEBOUNCE_MS));
        assert_eq!(session.body(), "precious\n");
        let text = "---\nfrom: jon@example.com\nto: [\"ada@example.com\"]\ncc: []\nbcc: []\nsubject: \"Hello\"\nin_reply_to: \"<abc@example.com>\"\nevil: 1\nattachments: []\n---\nprecious\n";
        session.push_disk(Some(text.into()), 1_000);
        assert!(session.poll_disk(1_000 + DEBOUNCE_MS));
        assert!(session.body().contains("precious"));
        session.mark_seen();
        session.digests.wait_idle();
        assert!(session.prepare_send().is_err());
    }

    #[test]
    fn in_reply_to_edits_are_ignored() {
        let (_temp, mut session) = temp_session("body\n");
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
        let (_temp, mut session) = temp_session("body\n");
        let original = session.serialize();
        let bad = original.replace(
            "to: [\"ada@example.com\"]",
            "to: [\"not-an-email\"]\nevil: 1",
        );
        session.edit(session.revision(), &original, &bad).unwrap();
        session.mark_seen();
        session.digests.wait_idle();
        assert!(session.prepare_send().is_err());
        session
            .set_addresses_from_user("to", "ada@example.com")
            .unwrap();
        session.mark_seen();
        session.digests.wait_idle();
        assert!(
            session.prepare_send().is_ok(),
            "{:?}",
            session.snapshot().issues
        );
    }

    #[test]
    fn edit_does_not_rewrite_a_line_the_person_extended() {
        let (_temp, mut session) = temp_session("Please send the report\n");
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
        let (_temp, mut session) = temp_session("alpha paragraph\n");
        session.set_body_from_user("user paragraph\n".into());
        let err = session
            .edit(1, "alpha paragraph", "agent paragraph")
            .unwrap_err();
        assert!(err.contains("user paragraph"));
        assert!(session.body().contains("user paragraph"));
    }

    #[test]
    fn user_changes_are_coalesced_for_the_agent() {
        let (_temp, mut session) = temp_session("alpha\n");
        assert!(session.take_user_changes("bot").is_empty());
        session.set_body_from_user("alpha!\n".into());
        session.set_body_from_user("alpha!!\n".into());
        let diff = session.take_user_changes("bot");
        assert!(diff.contains("alpha!!"), "{diff}");
        assert!(!diff.contains("alpha!\n-") && diff.matches("alpha!").count() >= 1);
    }

    #[test]
    fn replace_body_is_one_hunk() {
        let (_temp, mut session) = temp_session("short\n");
        session
            .replace_body("a full rewrite of the letter\n")
            .unwrap();
        assert_eq!(session.snapshot().unreviewed, 1);
        assert_eq!(session.snapshot().hunks.len(), 1);
    }

    #[test]
    fn a_hunk_maps_onto_its_own_place_in_the_body() {
        let link = "[pay](https://example.com/pay)";
        let (_temp, mut session) = temp_session(&format!("{link}\n\nlater\n"));
        let revision = session.revision();
        session.edit(revision, "later", link).unwrap();
        let snap = session.snapshot();
        let hunk = snap.hunks.first().expect("one hunk");
        let (start, end) = snap.body_range(hunk.start, hunk.end).unwrap();
        // The second copy, the one the agent wrote, not the first.
        assert_eq!(start, snap.body.rfind(link).unwrap());
        assert_eq!(&snap.body[start..end], link);
        // A range in the header is not in the body.
        assert_eq!(snap.body_range(0, 3), None);
        assert!(!snap.agent_authored, "a person's draft");
        session.set_owner("agent:bot");
        assert!(session.snapshot().agent_authored);
    }

    #[test]
    fn unreferenced_files_are_not_in_the_outgoing_message() {
        let (_temp, mut session) = temp_session("![chart](chart.png)\n");
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
        session.digests.wait_idle();
        let outgoing = session.prepare_send().unwrap();
        let mime = String::from_utf8(outgoing.mime).unwrap();
        assert!(mime.contains("chart.png"));
        assert!(!mime.contains("secret.txt"));
        assert!(!mime.contains("nope"));
    }

    #[test]
    fn removing_the_drafts_identity_blocks_send() {
        let (_temp, mut session) = temp_session("body\n");
        session.set_identities(vec![address_only("primary@example.com".into())]);
        session.mark_seen();
        session.digests.wait_idle();
        let err = session.prepare_send().unwrap_err();
        assert!(err.contains("from"), "{err}");
    }

    #[test]
    fn offline_edit_keeps_the_reply_lock_and_records_a_hunk() {
        let (temp, mut session) = temp_session("body\n");
        let db = temp.path().join("mail.db");
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
    }

    #[test]
    fn open_refuses_a_folder_with_no_mailbox_record() {
        if crate::test_support::in_isolated_process().is_none() {
            return;
        }
        let (_temp, session) = temp_session("body\n");
        let folder = session.folder().to_path_buf();
        drop(session);
        let err = match DraftSession::open_folder(&folder, AccountId("local".into())) {
            Ok(_) => panic!("opened a folder with no mailbox record"),
            Err(err) => err,
        };
        assert!(err.contains("mailbox"), "{err}");
    }

    #[test]
    fn control_files_are_outside_the_agent_folder() {
        let (temp, session) = temp_session("body\n");
        let folder = session.folder().to_path_buf();
        drop(session);
        assert!(!folder.join("account").exists());
        assert!(!folder.join("reply.lock").exists());
        assert!(!folder.join("identities").exists());
        std::fs::write(folder.join("account"), "not-the-account\n").unwrap();
        std::fs::write(folder.join("reply.lock"), "<evil@example.com>\n").unwrap();
        std::fs::write(folder.join("identities"), "evil@example.com\n").unwrap();
        let db = temp.path().join("mail.db");
        let store = crate::Store::open(&db).unwrap();
        store
            .upsert_account(&crate::Account {
                id: AccountId("local".into()),
                display_name: "Jon".into(),
                email: "jon@example.com".into(),
                backend_kind: crate::BackendKind::Bridge,
            })
            .unwrap();
        let mut live = DraftSession::testing_create_local(
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
        assert!(opened.identity("jon@example.com").is_some());
        assert!(opened.identity("evil@example.com").is_none());
    }

    #[test]
    fn reject_reverts_the_edited_range_not_an_earlier_copy() {
        let (_temp, mut session) = temp_session("alpha\nbeta alpha\n");
        session
            .edit(session.revision(), "beta alpha", "alpha")
            .unwrap();
        assert_eq!(session.body(), "alpha\nalpha\n");
        let hunk = session.snapshot().hunks[0].id;
        assert!(session.reject_hunk(hunk));
        assert_eq!(session.body(), "alpha\nbeta alpha\n");
    }

    #[test]
    fn a_sent_draft_is_locked_against_another_send() {
        let (_temp, _store, mut session) = stored_session("body\n");
        session.mark_seen();
        assert!(session.prepare_send().is_ok());
        session
            .begin_send(&crate::MessageId::new("test@compose.fell"))
            .unwrap();
        assert!(
            session
                .begin_send(&crate::MessageId::new("test@compose.fell"))
                .unwrap_err()
                .contains("being sent")
        );
        assert!(session.prepare_send().unwrap_err().contains("being sent"));
        session.end_send(None);
        assert!(session.prepare_send().is_ok());
        for (lock, needle) in [
            (SendLock::Sent, "was sent"),
            (SendLock::MaybeSent, "may already have been sent"),
        ] {
            // A recorded send is never cleared, so each case is a new draft.
            let (_temp, _store, mut session) = stored_session("body\n");
            session.mark_seen();
            session
                .begin_send(&crate::MessageId::new("test@compose.fell"))
                .unwrap();
            session.end_send(Some(lock));
            assert!(session.prepare_send().unwrap_err().contains(needle));
            assert!(
                session
                    .begin_send(&crate::MessageId::new("test@compose.fell"))
                    .unwrap_err()
                    .contains(needle)
            );
            assert_eq!(
                session.snapshot().send_block.as_deref(),
                Some(lock.reason())
            );
        }
    }

    #[test]
    fn send_blocks_until_the_painted_revision_is_marked_seen() {
        let (_temp, session) = temp_session("body\n");
        session.digests.wait_idle();
        let err = session.prepare_send().unwrap_err();
        assert!(err.contains("on screen"), "{err}");
    }

    #[test]
    fn undo_removes_an_agent_edit() {
        let (_temp, mut session) = temp_session("alpha\n");
        session.edit(session.revision(), "alpha", "beta").unwrap();
        assert!(session.body().contains("beta"));
        assert!(session.undo());
        assert!(session.body().contains("alpha"));
    }

    #[test]
    fn send_blocks_when_a_painted_attachment_changes() {
        let (_temp, mut session) = temp_session("![chart](chart.png)\n");
        fs::write(session.folder().join("chart.png"), b"one").unwrap();
        session.mark_seen();
        // The shown version is hashed off the editor's thread first.
        session.digests.wait_idle();
        assert!(session.prepare_send().is_ok());
        fs::write(session.folder().join("chart.png"), b"replaced").unwrap();
        session.mark_seen();
        session.digests.wait_idle();
        let err = session.prepare_send().unwrap_err();
        assert!(err.contains("attachment"), "{err}");
    }

    #[test]
    fn a_text_edit_does_not_accept_swapped_attachment_bytes() {
        let (_temp, mut session) = temp_session("![chart](chart.png)\n");
        let path = session.folder().join("chart.png");
        fs::write(&path, b"one").unwrap();
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        session.mark_seen();
        // The shown version is hashed off the editor's thread first.
        session.digests.wait_idle();
        assert!(session.prepare_send().is_ok());
        fs::write(&path, b"two").unwrap();
        let file = fs::File::options().write(true).open(&path).unwrap();
        file.set_modified(modified).unwrap();
        session.set_body_from_user("![chart](chart.png)\n\nmore\n".into());
        session.mark_seen();
        session.digests.wait_idle();
        let err = session.prepare_send().unwrap_err();
        assert!(err.contains("attachment"), "{err}");
        session.set_body_from_user("no chart\n".into());
        session.mark_seen();
        session.set_body_from_user("![chart](chart.png)\n".into());
        session.mark_seen();
        // The shown version is hashed off the editor's thread first.
        session.digests.wait_idle();
        assert!(session.prepare_send().is_ok());
    }

    // Restore write access before TempDir tries to remove the fixture.
    struct ReadOnlyDraft<'a>([&'a Path; 2]);

    impl<'a> ReadOnlyDraft<'a> {
        fn new(folder: &'a Path, control: &'a Path) -> Self {
            use std::os::unix::fs::PermissionsExt;

            let guard = Self([folder, control]);
            for path in guard.0 {
                fs::set_permissions(path, fs::Permissions::from_mode(0o500)).unwrap();
            }
            guard
        }
    }

    impl Drop for ReadOnlyDraft<'_> {
        fn drop(&mut self) {
            use std::os::unix::fs::PermissionsExt;

            for path in self.0 {
                let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
            }
        }
    }

    #[test]
    fn a_save_with_a_writer_does_not_touch_disk_on_the_calling_thread() {
        let (_temp, mut session) = temp_session("body\n");
        session.attach_writer();
        let folder = session.folder().to_path_buf();
        let control = workspace::control_dir(&folder);
        // Saves into a folder this thread cannot write. An inline save
        // would fail here; a queued one returns and reports later.
        let read_only = ReadOnlyDraft::new(&folder, &control);
        let readonly = fs::write(folder.join("probe"), "x").is_err();
        session.set_body_from_user("changed\n".into());
        assert!(session.flush().is_ok());
        let _ = session.save_waiter().unwrap().wait();
        drop(read_only);
        session.set_body_from_user("again\n".into());
        session.flush().unwrap();
        session.save_waiter().unwrap().wait().unwrap();
        if readonly {
            assert!(
                session
                    .snapshot()
                    .notices
                    .iter()
                    .any(|notice| notice.starts_with("could not save draft.md")),
                "{:?}",
                session.snapshot().notices
            );
        }
        let disk = fs::read_to_string(folder.join("draft.md")).unwrap();
        assert_eq!(disk, session.serialize());
    }

    #[test]
    fn queued_saves_land_in_order_and_the_newest_wins() {
        let (_temp, mut session) = temp_session("0\n");
        session.attach_writer();
        for step in 1..=50 {
            session.set_body_from_user(format!("{step}\n"));
            session.flush().unwrap();
        }
        session.save_waiter().unwrap().wait().unwrap();
        let disk = fs::read_to_string(session.folder().join("draft.md")).unwrap();
        assert_eq!(disk, session.serialize());
        assert!(disk.ends_with("50\n"), "{disk}");
    }

    #[test]
    fn a_queued_save_is_not_read_back_as_an_agent_edit() {
        let (_temp, mut session) = temp_session("first\n");
        session.attach_writer();
        session.set_body_from_user("second\n".into());
        session.flush().unwrap();
        let older = session.serialize();
        session.set_body_from_user("third\n".into());
        session.flush().unwrap();
        // The watcher can read the file while it still shows the older
        // save. That is Fell's own write, not an agent edit.
        session.push_disk(Some(older.clone()), 0);
        assert!(!session.poll_disk(10_000));
        assert!(session.snapshot().hunks.is_empty());
        session.save_waiter().unwrap().wait().unwrap();
        // Once the newer save is on disk, the older text from someone else
        // is a real edit again.
        assert!(!session.is_own_write(&older, session.disk_generation()));
    }

    #[test]
    fn a_failed_save_does_not_merge_the_bytes_still_on_disk() {
        let (_temp, mut session) = temp_session("alpha\n");
        session.attach_writer();
        let folder = session.folder().to_path_buf();
        session.set_body_from_user("beta\n".into());
        session.flush().unwrap();
        session.save_waiter().unwrap().wait().unwrap();
        let on_disk = fs::read_to_string(folder.join("draft.md")).unwrap();
        // The next save cannot replace draft.md.
        let control = workspace::control_dir(&folder);
        let read_only = ReadOnlyDraft::new(&folder, &control);
        let readonly = fs::write(folder.join("probe"), "x").is_err();
        session.set_body_from_user("gamma\n".into());
        session.flush().unwrap();
        let saved = session.save_waiter().unwrap().wait();
        drop(read_only);
        if readonly {
            assert!(
                saved.is_err(),
                "the agent reply must not claim the save landed"
            );
        }
        // The watcher reads what is really on disk: the earlier save.
        session.push_disk(Some(on_disk), 0);
        assert!(!session.poll_disk(10_000));
        assert!(session.snapshot().hunks.is_empty());
        assert!(session.body().starts_with("gamma"));
    }

    #[test]
    fn a_read_made_before_a_later_save_is_still_fells_own() {
        let (_temp, mut session) = temp_session("alpha\n");
        session.attach_writer();
        session.set_body_from_user("beta\n".into());
        session.flush().unwrap();
        session.save_waiter().unwrap().wait().unwrap();
        // The watcher samples, reads draft.md, then waits for the lock...
        let since = session.disk_generation();
        let read = fs::read_to_string(session.folder().join("draft.md")).unwrap();
        // ...while the editor saves again.
        session.set_body_from_user("gamma\n".into());
        session.flush().unwrap();
        session.save_waiter().unwrap().wait().unwrap();
        session.push_disk_seen(Some(read), since, 0);
        assert!(!session.poll_disk(10_000));
        assert!(session.snapshot().hunks.is_empty());
    }

    #[test]
    fn dropping_a_draft_writes_its_queued_save_first() {
        let (_temp, mut session) = temp_session("alpha\n");
        session.attach_writer();
        let folder = session.folder().to_path_buf();
        for step in 0..20 {
            session.set_body_from_user(format!("step {step}\n"));
            session.flush().unwrap();
        }
        let expected = session.serialize();
        drop(session);
        assert_eq!(
            fs::read_to_string(folder.join("draft.md")).unwrap(),
            expected
        );
    }

    fn stored_session(body: &str) -> (tempfile::TempDir, crate::Store, DraftSession) {
        let (temp, mut session) = temp_session(body);
        let db = temp.path().join("mail.db");
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
        session.flush().unwrap();
        (temp, store, session)
    }

    fn reopen(session: DraftSession, store: &crate::Store) -> DraftSession {
        let folder = session.folder().to_path_buf();
        drop(session);
        DraftSession::open_with_store(&folder, AccountId("local".into()), store).unwrap()
    }

    fn only_hunk(session: &DraftSession) -> u64 {
        let hunks = session.snapshot().hunks;
        assert_eq!(hunks.len(), 1, "{hunks:?}");
        hunks[0].id
    }

    #[test]
    fn painting_never_reads_an_attachment() {
        let (_temp, mut session) = temp_session("![chart](chart.png)\n");
        fs::write(session.folder().join("chart.png"), b"one").unwrap();
        session.mark_seen();
        for _ in 0..50 {
            let snap = session.snapshot();
            assert!(snap.send_block.is_none(), "{:?}", snap.send_block);
            session.mark_seen();
        }
        session.digests.wait_idle();
        // One read, on the hash thread, for the one version of the file.
        assert_eq!(session.digests.hashed(), 1);
        for _ in 0..50 {
            let _ = session.snapshot();
        }
        session.digests.wait_idle();
        assert_eq!(session.digests.hashed(), 1);
        session.digests.wait_idle();
        assert!(session.prepare_send().is_ok());
    }

    #[test]
    fn a_touched_attachment_with_the_same_bytes_still_sends() {
        let (_temp, mut session) = temp_session("![chart](chart.png)\n");
        let path = session.folder().join("chart.png");
        fs::write(&path, b"same bytes").unwrap();
        session.mark_seen();
        session.digests.wait_idle();
        fs::write(&path, b"same bytes").unwrap();
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(7))
            .unwrap();
        // The new version is unknown until the hash thread reads it.
        assert!(session.snapshot().send_block.is_some());
        session.digests.wait_idle();
        assert_eq!(session.snapshot().send_block, None);
        session.digests.wait_idle();
        assert!(session.prepare_send().is_ok());
    }

    #[test]
    fn send_blocks_when_painted_bytes_change_under_the_same_stamp() {
        // A rewrite inside one timestamp tick leaves the stamp alone. The
        // digest taken for that stamp still catches it at send.
        let (_temp, mut session) = temp_session("![chart](chart.png)\n");
        fs::write(session.folder().join("chart.png"), b"on screen").unwrap();
        session.mark_seen();
        session.digests.wait_idle();
        let (shown, stamp) = session.painted_files.clone().unwrap().remove(0);
        session.digests.forget(&shown);
        session
            .digests
            .record(&shown, stamp, digest::sha256(b"what the person saw"));
        assert_eq!(session.snapshot().send_block, None);
        session.digests.wait_idle();
        let err = session.prepare_send().unwrap_err();
        assert!(err.contains("attachment"), "{err}");
    }

    #[test]
    fn the_files_handed_to_send_are_the_bytes_that_were_checked() {
        let (_temp, mut session) = temp_session("![chart](chart.png)\n");
        let path = session.folder().join("chart.png");
        fs::write(&path, b"checked").unwrap();
        session.mark_seen();
        session.digests.wait_idle();
        let outgoing = session.prepare_send().unwrap();
        assert!(!outgoing.mime.is_empty());
        fs::write(&path, b"swapped after the check").unwrap();
        let files = session.resolved_files();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].bytes, b"checked");
    }

    #[test]
    fn a_hard_linked_attachment_blocks_send() {
        let (temp, mut session) = temp_session("![chart](chart.png)\n");
        let outside = temp.path().join("outside-secret.txt");
        fs::write(&outside, b"secret").unwrap();
        fs::hard_link(&outside, session.folder().join("chart.png")).unwrap();
        session.mark_seen();
        let snap = session.snapshot();
        assert!(snap.attachments.is_empty(), "{:?}", snap.attachments);
        let block = snap.send_block.unwrap_or_default();
        assert!(block.contains("hard link"), "{block}");
        session.digests.wait_idle();
        let err = session.prepare_send().unwrap_err();
        assert!(err.contains("hard link"), "{err}");
    }

    #[test]
    fn reject_follows_the_hunk_past_edits_before_it() {
        let (_temp, mut session) = temp_session("alpha\nbeta\n");
        session.edit(session.revision(), "beta", "gamma").unwrap();
        let id = only_hunk(&session);
        // The person writes the same word earlier in the draft, so the
        // text alone would not say which `gamma` is the agent's.
        session.set_body_from_user("gamma first\nalpha\ngamma\n".into());
        session.end_user_change();
        assert!(session.reject_hunk(id));
        assert_eq!(session.body(), "gamma first\nalpha\nbeta\n");
    }

    #[test]
    fn an_agent_edit_before_a_hunk_moves_it() {
        let (_temp, mut session) = temp_session("alpha\nbeta\nend\n");
        session.edit(session.revision(), "beta", "BETA").unwrap();
        session
            .edit(session.revision(), "alpha", "a much longer alpha")
            .unwrap();
        let hunks = session.snapshot().hunks;
        assert_eq!(hunks.len(), 2, "{hunks:?}");
        let text = session.serialize();
        for hunk in &hunks {
            assert!(
                ["BETA", "a much longer alpha"].contains(&&text[hunk.start..hunk.end]),
                "{hunk:?}"
            );
        }
        let beta = hunks
            .iter()
            .find(|hunk| &text[hunk.start..hunk.end] == "BETA")
            .unwrap()
            .id;
        assert!(session.reject_hunk(beta));
        assert_eq!(session.body(), "a much longer alpha\nbeta\nend\n");
        let alpha = only_hunk(&session);
        assert!(session.reject_hunk(alpha));
        assert_eq!(session.body(), "alpha\nbeta\nend\n");
        assert!(session.snapshot().hunks.is_empty());
    }

    #[test]
    fn a_header_edit_by_the_person_moves_a_body_hunk() {
        let (_temp, mut session) = temp_session("alpha\nbeta\n");
        session.edit(session.revision(), "beta", "BETA").unwrap();
        let id = only_hunk(&session);
        session.set_subject_from_user("A much longer subject line".into());
        session
            .set_addresses_from_user("to", "ada@example.com, bob@example.com")
            .unwrap();
        session.end_user_change();
        let snap = session.snapshot();
        assert_eq!(&snap.text[snap.hunks[0].start..snap.hunks[0].end], "BETA");
        assert!(session.reject_hunk(id));
        assert_eq!(session.body(), "alpha\nbeta\n");
        assert_eq!(session.fields().subject, "A much longer subject line");
    }

    #[test]
    fn an_agent_edit_over_its_own_edit_rejects_to_the_persons_text() {
        let (_temp, mut session) = temp_session("see you Monday\n");
        session
            .edit(session.revision(), "Monday", "Tuesday")
            .unwrap();
        session
            .edit(session.revision(), "you Tuesday", "you on Wednesday")
            .unwrap();
        let id = only_hunk(&session);
        assert!(session.reject_hunk(id));
        assert_eq!(session.body(), "see you Monday\n");
    }

    #[test]
    fn typing_over_an_agent_edit_makes_it_the_persons() {
        let (_temp, mut session) = temp_session("alpha\nbeta\n");
        session
            .edit(session.revision(), "beta", "agent words")
            .unwrap();
        let id = only_hunk(&session);
        session.set_body_from_user("alpha\nagent's own words\n".into());
        session.end_user_change();
        assert!(session.snapshot().hunks.is_empty());
        assert!(!session.reject_hunk(id));
        assert_eq!(session.body(), "alpha\nagent's own words\n");
    }

    #[test]
    fn undo_takes_back_the_agent_edit_and_nothing_typed_after_it() {
        let (_temp, mut session) = temp_session("alpha\nbeta\n");
        // The person is mid-word: their undo step is still open.
        session.set_body_from_user("alpha typed\nbeta\n".into());
        session.edit(session.revision(), "beta", "BETA").unwrap();
        session.set_body_from_user("alpha typed more\nBETA\n".into());
        assert!(session.undo());
        assert_eq!(session.body(), "alpha typed\nBETA\n");
        assert!(session.undo());
        assert_eq!(session.body(), "alpha typed\nbeta\n");
        assert!(session.snapshot().hunks.is_empty());
    }

    #[test]
    fn undo_takes_back_a_file_edit_on_its_own() {
        let (_temp, mut session) = temp_session("alpha\n\nbeta\n");
        session.set_body_from_user("alpha!\n\nbeta\n".into());
        let agent = session.serialize().replace("beta", "BETA");
        session.push_disk(Some(agent), 0);
        assert!(session.poll_disk(DEBOUNCE_MS));
        assert!(session.body().contains("BETA"));
        assert!(session.undo());
        assert_eq!(session.body(), "alpha!\n\nbeta\n");
    }

    #[test]
    fn hunks_survive_a_reopen_from_the_mailbox() {
        let (_temp, store, mut session) = stored_session("alpha\nbeta\n");
        session.edit(session.revision(), "beta", "BETA").unwrap();
        session.flush().unwrap();
        let mut session = reopen(session, &store);
        let id = only_hunk(&session);
        assert!(session.reject_hunk(id));
        assert_eq!(session.body(), "alpha\nbeta\n");
    }

    #[test]
    fn a_tampered_hunk_file_is_not_trusted() {
        let (_temp, store, mut session) = stored_session("Pay invoice 12 today.\nThanks\n");
        session
            .edit(session.revision(), "Thanks", "Cheers")
            .unwrap();
        session.flush().unwrap();
        let control = workspace::control_dir(session.folder());
        let text = session.serialize();
        let at = text.find("Pay invoice 12").unwrap();
        // Hide the real agent change and dress up the person's sentence as
        // an agent edit whose reject would write the attacker's text.
        let forged = serde_json::json!([{
            "id": 99,
            "kind": "body",
            "previous": "Pay invoice 12 to account 666.",
            "inserted": "Pay invoice 12 today.",
            "at": at,
        }]);
        fs::create_dir_all(&control).unwrap();
        fs::write(control.join("hunks.json"), forged.to_string()).unwrap();
        let mut session = reopen(session, &store);
        let hunks = session.snapshot().hunks;
        assert_eq!(hunks.len(), 1, "{hunks:?}");
        assert_ne!(hunks[0].id, 99);
        assert_eq!(&session.serialize()[hunks[0].start..hunks[0].end], "Cheers");
        assert!(!session.reject_hunk(99));
        assert!(session.reject_hunk(hunks[0].id));
        assert_eq!(session.body(), "Pay invoice 12 today.\nThanks\n");
    }

    #[test]
    fn a_draft_file_edit_while_closed_is_marked_for_review() {
        let (_temp, store, session) = stored_session("alpha\nbeta\n");
        let folder = session.folder().to_path_buf();
        let text = session.serialize();
        drop(session);
        fs::write(folder.join("draft.md"), text.replace("beta", "BETA")).unwrap();
        let mut session =
            DraftSession::open_with_store(&folder, AccountId("local".into()), &store).unwrap();
        let id = only_hunk(&session);
        assert!(session.reject_hunk(id));
        assert_eq!(session.body(), "alpha\nbeta\n");
    }

    #[test]
    fn an_edit_while_closed_cannot_hide_an_earlier_agent_change() {
        let (_temp, store, mut session) = stored_session("alpha\nbeta\n");
        session.edit(session.revision(), "beta", "BETA").unwrap();
        session.flush().unwrap();
        let folder = session.folder().to_path_buf();
        let text = session.serialize();
        drop(session);
        fs::write(folder.join("draft.md"), text.replace("BETA", "BETA!")).unwrap();
        let mut session =
            DraftSession::open_with_store(&folder, AccountId("local".into()), &store).unwrap();
        let id = only_hunk(&session);
        assert!(session.reject_hunk(id));
        assert_eq!(session.body(), "alpha\nbeta\n");
    }

    #[test]
    fn settling_the_hub_waits_for_every_draft() {
        let (_temp, session) = temp_session("alpha\n");
        let folder = session.folder().to_path_buf();
        let id = session.id().to_string();
        let hub = super::super::DraftHub::new();
        hub.insert(session);
        let expected = hub
            .with_session(&id, |session| {
                session.set_body_from_user("on quit\n".into());
                session.flush().unwrap();
                session.serialize()
            })
            .unwrap();
        hub.settle_all();
        assert_eq!(
            fs::read_to_string(folder.join("draft.md")).unwrap(),
            expected
        );
    }

    #[test]
    fn a_draft_from_before_stored_review_asks_to_be_read() {
        let (_temp, store, session) = stored_session("alpha\nbeta\n");
        let control = workspace::control_dir(session.folder());
        fs::create_dir_all(&control).unwrap();
        fs::write(control.join("hunks.json"), "[]").unwrap();
        store
            .connection()
            .execute("DELETE FROM compose_review", [])
            .unwrap();
        let session = reopen(session, &store);
        assert!(session.snapshot().hunks.is_empty());
        assert!(
            session
                .snapshot()
                .notices
                .iter()
                .any(|notice| notice.contains("read this draft")),
            "{:?}",
            session.snapshot().notices
        );
    }

    #[test]
    fn a_file_edit_during_an_insert_is_its_own_undo_step() {
        let (_temp, mut session) = temp_session("First paragraph.\n\nSecond paragraph.\n");
        session.begin_user_change();
        session.set_body_from_user("First paragraph. Typed.\n\nSecond paragraph.\n".into());
        // An agent's script rewrites the other paragraph while the person
        // is still in insert mode.
        let mut disk = session.serialize();
        disk = disk.replace("Second paragraph.", "Second paragraph, edited.");
        session.push_disk(Some(disk), 0);
        assert!(session.poll_disk(10_000));
        assert!(session.body().contains("Typed."));
        assert!(session.body().contains("edited."));
        session.end_user_change();
        // One undo removes only the file edit...
        assert!(session.undo());
        assert!(session.body().contains("Typed."), "{}", session.body());
        assert!(!session.body().contains("edited."), "{}", session.body());
        // ...the next one the typing.
        assert!(session.undo());
        assert!(!session.body().contains("Typed."), "{}", session.body());
    }

    #[test]
    fn a_draft_cannot_change_while_it_is_sent() {
        let (_temp, _store, mut session) = stored_session("body\n");
        session
            .begin_send(&crate::MessageId::new("test@compose.fell"))
            .unwrap();
        let before = session.serialize();
        session.set_body_from_user("typed during send\n".into());
        session.set_subject_from_user("changed".into());
        assert!(
            session
                .set_addresses_from_user("to", "eve@example.com")
                .is_err()
        );
        assert!(session.edit(session.revision(), "body", "agent").is_err());
        assert!(session.replace_body("agent rewrite\n").is_err());
        assert!(!session.undo());
        let mut disk = before.clone();
        disk = disk.replace("body", "file edit");
        session.push_disk(Some(disk), 0);
        session.poll_disk(10_000);
        assert_eq!(session.serialize(), before);
        // A send that failed before the server had it opens the draft again.
        session.end_send(None);
        session.set_body_from_user("now editable\n".into());
        assert_eq!(session.body(), "now editable\n");
    }

    #[test]
    fn a_claim_without_end_send_reopens_as_maybe_sent() {
        let (_temp, store, mut session) = stored_session("body\n");
        session.mark_seen();
        session
            .begin_send(&crate::MessageId::new("test@compose.fell"))
            .unwrap();
        // Fell stops mid-send: end_send never runs.
        let reopened = reopen(session, &store);
        assert_eq!(reopened.send_lock(), Some(SendLock::MaybeSent));
        assert!(
            reopened
                .prepare_send()
                .unwrap_err()
                .contains("may already have been sent")
        );
    }

    #[test]
    fn deleting_the_control_folder_does_not_unlock_a_sent_draft() {
        let (_temp, store, mut session) = stored_session("body\n");
        session.mark_seen();
        session
            .begin_send(&crate::MessageId::new("test@compose.fell"))
            .unwrap();
        session.end_send(Some(SendLock::Sent));
        let folder = session.folder().to_path_buf();
        let control = workspace::control_dir(&folder);
        drop(session);
        // An agent, or anything else that can write the draft folder,
        // removes everything Fell keeps beside the draft.
        fs::remove_dir_all(&control).unwrap();
        let mut reopened =
            DraftSession::open_with_store(&folder, AccountId("local".into()), &store).unwrap();
        assert_eq!(reopened.send_lock(), Some(SendLock::Sent));
        reopened.mark_seen();
        assert!(
            reopened
                .begin_send(&crate::MessageId::new("test@compose.fell"))
                .unwrap_err()
                .contains("was sent")
        );
    }

    #[test]
    fn a_send_that_transmitted_nothing_reopens_unlocked() {
        let (_temp, store, mut session) = stored_session("body\n");
        session
            .begin_send(&crate::MessageId::new("test@compose.fell"))
            .unwrap();
        session.end_send(None);
        let mut reopened = reopen(session, &store);
        assert_eq!(reopened.send_lock(), None);
        reopened
            .begin_send(&crate::MessageId::new("test@compose.fell"))
            .unwrap();
    }

    #[test]
    fn a_draft_with_no_mailbox_is_not_claimed() {
        if crate::test_support::in_isolated_process().is_none() {
            return;
        }
        let (temp, mut session) = temp_session("body\n");
        session.store_path = None;
        let err = session
            .begin_send(&crate::MessageId::new("test@compose.fell"))
            .unwrap_err();
        assert!(err.contains("could not record this send"), "{err}");
        assert_eq!(session.send_lock(), None);
        // A mailbox path that is gone is not created by the claim.
        let missing = temp.path().join("gone.db");
        session.bind_store(&missing);
        assert!(
            session
                .begin_send(&crate::MessageId::new("test@compose.fell"))
                .is_err()
        );
        assert!(!missing.exists());
    }

    #[test]
    fn a_send_the_mailbox_recorded_locks_a_session_that_missed_it() {
        let (_temp, store, mut session) = stored_session("body\n");
        store
            .finish_compose_send(&AccountId("local".into()), "d1", Some(SendLock::Sent))
            .unwrap();
        // This session was opened before the record was written.
        assert!(
            session
                .begin_send(&crate::MessageId::new("test@compose.fell"))
                .unwrap_err()
                .contains("was sent")
        );
        assert_eq!(session.send_lock(), Some(SendLock::Sent));
    }

    #[test]
    fn an_old_send_state_file_is_migrated_once() {
        for (old, lock) in [
            ("", SendLock::MaybeSent),
            ("se", SendLock::MaybeSent),
            ("garbage", SendLock::MaybeSent),
            ("maybe-sent", SendLock::MaybeSent),
            ("sent", SendLock::Sent),
        ] {
            let (_temp, store, session) = stored_session("body\n");
            let folder = session.folder().to_path_buf();
            drop(session);
            // The first open by this build already happened; start as if
            // the draft came from an older Fell.
            store.conn.execute("DELETE FROM compose_send", []).unwrap();
            let file = workspace::control_dir(&folder).join(LEGACY_SEND_STATE);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(&file, old).unwrap();
            let reopened =
                DraftSession::open_with_store(&folder, AccountId("local".into()), &store).unwrap();
            assert_eq!(reopened.send_lock(), Some(lock), "{old:?}");
            assert!(!file.exists(), "{old:?}");
            drop(reopened);
            let reopened =
                DraftSession::open_with_store(&folder, AccountId("local".into()), &store).unwrap();
            assert_eq!(reopened.send_lock(), Some(lock), "{old:?}");
        }
    }

    #[test]
    fn a_send_state_file_written_after_migration_is_ignored() {
        let (_temp, store, session) = stored_session("body\n");
        let folder = session.folder().to_path_buf();
        let reopened = reopen(session, &store);
        drop(reopened);
        let file = workspace::control_dir(&folder).join(LEGACY_SEND_STATE);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, "sent").unwrap();
        let reopened =
            DraftSession::open_with_store(&folder, AccountId("local".into()), &store).unwrap();
        assert_eq!(reopened.send_lock(), None);
    }

    #[test]
    fn unlocking_releases_only_a_draft_that_may_have_been_sent() {
        let (_temp, store, session) = stored_session("body\n");
        let account = AccountId("local".into());
        store
            .finish_compose_send(&account, "d1", Some(SendLock::MaybeSent))
            .unwrap();
        let mut reopened = reopen(session, &store);
        assert_eq!(reopened.send_lock(), Some(SendLock::MaybeSent));
        reopened.release_maybe_sent().unwrap();
        assert_eq!(reopened.send_lock(), None);
        // The mailbox agrees, so a restart does not lock it again.
        let mut reopened = reopen(reopened, &store);
        assert_eq!(reopened.send_lock(), None);
        assert!(reopened.release_maybe_sent().is_err(), "nothing to unlock");
        // A recorded send is never released.
        store
            .finish_compose_send(&account, "d1", Some(SendLock::Sent))
            .unwrap();
        let mut reopened = reopen(reopened, &store);
        assert!(reopened.release_maybe_sent().is_err());
        assert_eq!(reopened.send_lock(), Some(SendLock::Sent));
    }

    #[test]
    fn a_late_clear_never_unlocks_a_sent_draft() {
        let (_temp, store, session) = stored_session("body\n");
        let account = AccountId("local".into());
        store
            .finish_compose_send(&account, "d1", Some(SendLock::Sent))
            .unwrap();
        // A second session's "nothing went out" and a stray "may have been
        // sent" both arrive after the send was recorded.
        store.finish_compose_send(&account, "d1", None).unwrap();
        store
            .finish_compose_send(&account, "d1", Some(SendLock::MaybeSent))
            .unwrap();
        assert_eq!(
            store.compose_send_record(&account, "d1").unwrap(),
            Some(Some(SendLock::Sent))
        );
        let reopened = reopen(session, &store);
        assert_eq!(reopened.send_lock(), Some(SendLock::Sent));
    }

    #[test]
    fn a_migration_racing_another_keeps_the_sent_record() {
        let (_temp, store, session) = stored_session("body\n");
        let account = AccountId("local".into());
        let folder = session.folder().to_path_buf();
        drop(session);
        store.conn.execute("DELETE FROM compose_send", []).unwrap();
        // The first open moved `sent` out of the file and deleted it.
        assert_eq!(
            store
                .migrate_compose_send(&account, "d1", Some(SendLock::Sent))
                .unwrap(),
            Some(SendLock::Sent)
        );
        // A second open that raced it finds no file and would say "clear".
        assert_eq!(
            store.migrate_compose_send(&account, "d1", None).unwrap(),
            Some(SendLock::Sent)
        );
        let reopened = DraftSession::open_with_store(&folder, account, &store).unwrap();
        assert_eq!(reopened.send_lock(), Some(SendLock::Sent));
    }

    #[test]
    fn a_reopen_during_a_send_keeps_the_live_claim() {
        let (_temp, store, mut live) = stored_session("body\n");
        let folder = live.folder().to_path_buf();
        live.begin_send(&crate::MessageId::new("test@compose.fell"))
            .unwrap();
        let mut reopened =
            DraftSession::open_with_store(&folder, AccountId("local".into()), &store).unwrap();
        assert_eq!(
            reopened.send_lock(),
            Some(SendLock::MaybeSent),
            "the mailbox record"
        );
        reopened.inherit_send_lock(&live);
        assert_eq!(reopened.send_lock(), Some(SendLock::InFlight));
    }

    #[test]
    fn a_sent_confirmation_wins_over_an_open_unlock_dialog_and_a_lost_smtp_reply() {
        let (_temp, store, mut live) = stored_session("body\n");
        let id = crate::MessageId::new("submitted@compose.fell");
        live.begin_send(&id).unwrap();
        let account = live.account_id().clone();
        let mut reopened =
            DraftSession::open_with_store(live.folder(), account.clone(), &store).unwrap();
        assert_eq!(reopened.send_lock(), Some(SendLock::MaybeSent));
        store
            .apply_delta(
                &account,
                &crate::MailboxDelta {
                    roles: Some(crate::FolderRoles::from_names(&["Sent".into()])),
                    removals: vec![crate::FolderRemoval {
                        mailbox: "Sent".into(),
                        folder: "Sent".into(),
                        kind: crate::RemovalKind::Listing {
                            uidvalidity: 1,
                            present: vec![7],
                            through: 7,
                            found: vec![(7, id)],
                            sweep: false,
                        },
                    }],
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(live.send_lock(), Some(SendLock::InFlight));
        live.end_send(Some(SendLock::MaybeSent));
        assert_eq!(live.send_lock(), Some(SendLock::Sent));

        assert!(
            reopened
                .release_maybe_sent()
                .unwrap_err()
                .contains("was sent")
        );
        assert_eq!(reopened.send_lock(), Some(SendLock::Sent));
        assert_eq!(reopen(live, &store).send_lock(), Some(SendLock::Sent));
    }
}
