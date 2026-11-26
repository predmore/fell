//! Server actions waiting to be sent: the durable write-back outbox.
//!
//! Archive, move, delete to Trash, and spam change the local store at once.
//! When the account has `write_back = true` in `account.toml`, the same
//! [`WriteTx`] also writes one outbox row per server copy the change moved
//! (from `message_copies`), so the window stays instant and a queued action
//! survives a restart. The
//! sync thread replays the rows on its own `SELECT` session
//! ([`crate::AccountConfig::replay_outbox`]); the read-only sync session
//! never sends a write.
//!
//! A row names its copy by mailbox, UIDVALIDITY, and UID. The UIDVALIDITY is
//! checked after `SELECT`: a mailbox the server reset is a different set of
//! messages, so the row is dropped and never applied to another message.
//! A row whose copy is not on the server yet (the thread was moved again
//! before the first move was sent) has no UID; replay finds it in
//! `message_copies` once the earlier move, or a sync pass, has recorded it.
//! Rows for one message replay in the order they were queued.
//!
//! Undo of an action removes its rows that were not sent. Rows the server
//! already applied stay, and undo queues the reverse move for each. A row
//! sent but not yet settled (its reply was lost, or the copy it placed is
//! still being looked for) stays too, and undo queues a reverse that
//! follows it: once the row is done, the reverse moves the copy it placed
//! back. If the server refuses the row, it never moved the message, and
//! both rows are retired without a notice.
//!
//! Moves go out only as `UID MOVE` (RFC 6851). A server without MOVE keeps
//! the move in Fell. Bridge labeling separately copies to a label mailbox
//! and uses UID EXPUNGE only inside that mailbox.
//!
//! Turning write-back off starts a new generation, and rows not sent yet
//! are deleted. No row from an earlier generation is ever sent or
//! reversed, even when write-back is turned on again or a command on the
//! wire when it went off is answered late.
//!
//! Label, read, and star edits share the queue. Read and star keep one pending value per copy
//! and flag. A permanent delete from Trash is never queued.
//!
//! [`WriteTx`]: crate::store::WriteTx

use rusqlite::{Connection, OptionalExtension, params};

use crate::account::AccountId;
use crate::folders::FolderRoles;
use crate::store::Store;
use crate::{Error, Result};

pub(crate) mod drafts;
pub(crate) mod flags;
pub(crate) mod labels;

const OUTBOX_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS write_back_accounts (
    account_id TEXT PRIMARY KEY,
    enabled INTEGER NOT NULL,
    -- Bumped each time write-back is turned off; see the module doc.
    generation INTEGER NOT NULL DEFAULT 0,
    -- 1 once the user was told the server has no MOVE.
    move_missing INTEGER NOT NULL DEFAULT 0
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS server_outbox (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    account_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    state TEXT NOT NULL,
    -- The local action undo reverses; agent label actions use negative IDs.
    -- NULL for undo's own writes.
    action_id INTEGER,
    -- The stored row this server copy backs.
    message_id TEXT NOT NULL,
    source_folder TEXT NOT NULL,
    -- Raw (modified UTF-7) mailbox name. NULL: found at replay.
    source_mailbox TEXT,
    -- Both NULL: the copy is found at replay, once it is on the server.
    uidvalidity INTEGER,
    uid INTEGER,
    target_folder TEXT NOT NULL,
    -- Raw name. NULL: found in the server's LIST at replay.
    target_mailbox TEXT,
    -- Where the server put the message (COPYUID), once known.
    placed_uidvalidity INTEGER,
    placed_uid INTEGER,
    attempts INTEGER NOT NULL DEFAULT 0,
    retry_unix INTEGER NOT NULL DEFAULT 0,
    error TEXT NOT NULL DEFAULT '',
    created_unix INTEGER NOT NULL,
    settled_unix INTEGER,
    -- write_back_accounts.generation when the row was queued.
    generation INTEGER NOT NULL DEFAULT 0,
    -- 1 once the write for this row may have reached the server.
    sent INTEGER NOT NULL DEFAULT 0,
    -- The target's UIDVALIDITY and UIDNEXT just before the first MOVE was
    -- sent: a copy found there later counts only at or above this UIDNEXT.
    -- 0 when unknown.
    target_uidvalidity INTEGER NOT NULL DEFAULT 0,
    target_uidnext INTEGER NOT NULL DEFAULT 0,
    -- Exclusive upper UID bound from the most recent MOVE recovery check.
    -- NULL before a usable UIDNEXT is observed, including on older rows.
    move_recovery_end INTEGER,
    -- Undo's reverse of a row that was sent but not settled: that row's
    -- id. The reverse moves the copy that row places. 0 otherwise.
    reverses INTEGER NOT NULL DEFAULT 0,
    flag_value INTEGER,
    flag_modseq BLOB,
    -- Known value of the targeted flag at flag_modseq.
    flag_base INTEGER,
    -- An earlier flag write may have reached the server. Recover its result
    -- before replaying the replacement, without sending the old value again.
    flag_superseded INTEGER NOT NULL DEFAULT 0,
    -- Extra copies bound from an early keyword edit replay with that edit.
    keyword_parent INTEGER,
    append_mime BLOB,
    draft_id TEXT,
    draft_digest BLOB,
    append_checks INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_server_outbox_state ON server_outbox(account_id, state, id);
CREATE INDEX IF NOT EXISTS idx_server_outbox_action ON server_outbox(action_id);
CREATE INDEX IF NOT EXISTS idx_server_outbox_message ON server_outbox(account_id, message_id, id);
";

/// Columns added after the tables first shipped on a branch build.
const ADDED_COLUMNS: &[(&str, &str, &str)] = &[
    ("server_outbox", "keyword_parent", "INTEGER"),
    (
        "write_back_accounts",
        "generation",
        "INTEGER NOT NULL DEFAULT 0",
    ),
    (
        "write_back_accounts",
        "move_missing",
        "INTEGER NOT NULL DEFAULT 0",
    ),
    ("server_outbox", "generation", "INTEGER NOT NULL DEFAULT 0"),
    ("server_outbox", "sent", "INTEGER NOT NULL DEFAULT 0"),
    (
        "server_outbox",
        "target_uidvalidity",
        "INTEGER NOT NULL DEFAULT 0",
    ),
    (
        "server_outbox",
        "target_uidnext",
        "INTEGER NOT NULL DEFAULT 0",
    ),
    ("server_outbox", "move_recovery_end", "INTEGER"),
    ("server_outbox", "reverses", "INTEGER NOT NULL DEFAULT 0"),
    ("server_outbox", "flag_value", "INTEGER"),
    ("server_outbox", "flag_modseq", "BLOB"),
    ("server_outbox", "flag_base", "INTEGER"),
    ("server_outbox", "append_mime", "BLOB"),
    ("server_outbox", "draft_id", "TEXT"),
    ("server_outbox", "draft_digest", "BLOB"),
    ("server_draft_copies", "message_id", "TEXT"),
    (
        "server_outbox",
        "append_checks",
        "INTEGER NOT NULL DEFAULT 0",
    ),
    (
        "server_outbox",
        "flag_superseded",
        "INTEGER NOT NULL DEFAULT 0",
    ),
];

/// Sent and dropped rows are kept this long, so undo can still reverse a
/// move the server applied.
const KEEP_SETTLED_SECS: i64 = 30 * 24 * 60 * 60;

/// What an outbox row asks the server to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutboxKind {
    /// Move one copy from `source` to `target` (archive, move, delete to
    /// Trash, spam, and not spam).
    Move,
    AppendSent,
    AppendDraft,
    DeleteDraft,
    Seen,
    Flagged,
    Keyword,
    BridgeLabel,
}

impl OutboxKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Move => "move",
            Self::AppendSent => "append_sent",
            Self::AppendDraft => "append_draft",
            Self::DeleteDraft => "delete_draft",
            Self::Seen => "seen",
            Self::Flagged => "flagged",
            Self::Keyword => "keyword",
            Self::BridgeLabel => "bridge_label",
        }
    }

    fn parse(raw: &str) -> Result<Self> {
        match raw {
            "move" => Ok(Self::Move),
            "append_sent" => Ok(Self::AppendSent),
            "append_draft" => Ok(Self::AppendDraft),
            "delete_draft" => Ok(Self::DeleteDraft),
            "seen" => Ok(Self::Seen),
            "flagged" => Ok(Self::Flagged),
            "keyword" => Ok(Self::Keyword),
            "bridge_label" => Ok(Self::BridgeLabel),
            other => Err(Error::Action(format!("unknown outbox action {other}"))),
        }
    }

    pub(crate) fn is_append(self) -> bool {
        matches!(self, Self::AppendSent | Self::AppendDraft)
    }

    pub(crate) fn flag_name(self) -> Option<&'static str> {
        match self {
            Self::Move
            | Self::AppendSent
            | Self::AppendDraft
            | Self::DeleteDraft
            | Self::Keyword
            | Self::BridgeLabel => None,
            Self::Seen => Some("\\Seen"),
            Self::Flagged => Some("\\Flagged"),
        }
    }
}

/// Where an outbox row is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutboxState {
    /// Not sent yet, or sent and to be retried.
    Pending,
    /// The server applied it.
    Done,
    /// Never applied: see `error`.
    Dropped,
    /// Copied by an earlier build of write-back on a server without MOVE,
    /// with the source copy not yet expunged. Fell no longer copies, so it
    /// never finishes one: reported once, then Dropped.
    Abandoned,
}

impl OutboxState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Done => "done",
            Self::Dropped => "dropped",
            Self::Abandoned => "abandoned",
        }
    }

    fn parse(raw: &str) -> Result<Self> {
        match raw {
            "pending" => Ok(Self::Pending),
            // An earlier build's half-done copy; `ensure_schema` renames it.
            "copied" => Ok(Self::Abandoned),
            "done" => Ok(Self::Done),
            "dropped" => Ok(Self::Dropped),
            "abandoned" => Ok(Self::Abandoned),
            other => Err(Error::Action(format!("unknown outbox state {other}"))),
        }
    }
}

/// One server copy an action affects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxEntry {
    pub id: i64,
    pub kind: OutboxKind,
    pub state: OutboxState,
    pub action_id: Option<i64>,
    pub message_id: String,
    pub source_folder: String,
    pub source_mailbox: Option<String>,
    pub uidvalidity: Option<u32>,
    pub uid: Option<u32>,
    pub target_folder: String,
    pub target_mailbox: Option<String>,
    pub placed_uidvalidity: Option<u32>,
    pub placed_uid: Option<u32>,
    pub attempts: u32,
    pub retry_unix: i64,
    pub error: String,
    /// The write-back generation the row was queued in.
    pub generation: i64,
    /// The write for this row may have reached the server.
    pub sent: bool,
    /// The destination's UID state before sending the write.
    pub(crate) target_bound: Option<Bound>,
    pub(crate) move_recovery_end: Option<u32>,
    /// Undo's reverse of the row with this id, which was sent but not yet
    /// settled: this row moves the copy that one places.
    pub reverses: Option<i64>,
    /// Desired presence of the flag or label.
    pub flag_value: Option<bool>,
    pub(crate) flag_modseq: Option<u64>,
    pub(crate) flag_base: Option<bool>,
}

/// What one replay pass did ([`crate::AccountConfig::replay_outbox`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReplayReport {
    /// Rows the server applied.
    pub sent: usize,
    /// Rows still queued: refused for now, waiting on an earlier row, or
    /// not due yet.
    pub waiting: usize,
    /// Rows given up on and what is known about their server copies.
    pub dropped: Vec<DroppedWrite>,
    /// A previous flag write reached the retry limit without confirmation.
    /// The newer edit still waits for that confirmation.
    pub flag_recovery_delayed: bool,
    /// An APPEND failed and the Sent copy is still queued.
    pub sent_append_delayed: bool,
    /// A draft's upload first failed and is still queued.
    pub draft_append_delayed: bool,
    /// A draft upload that had been delayed is now confirmed.
    pub draft_append_recovered: bool,
    /// Replay applied a conflicting server value to the local store.
    pub flags_changed: bool,
    /// The connection failed part way; the rest waits for the next pass.
    pub interrupted: Option<String>,
}

/// A write that replay gave up on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DroppedWrite {
    pub kind: OutboxKind,
    pub source_folder: String,
    pub target_folder: String,
    pub reason: DropReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropReason {
    /// The source mailbox's UIDVALIDITY changed: its UIDs name other
    /// messages now, so the move is never applied.
    FolderReset,
    /// The server kept refusing the move.
    Refused,
    /// The result of a Sent APPEND could not be verified.
    UnconfirmedAppend,
    /// The destination copy of a possibly applied MOVE could not be verified.
    UnconfirmedMove,
    /// The MOVE could not be verified, but the source copy was restored in Fell.
    MoveRestored,
    /// The MOVE could not be verified and the original UID is gone.
    MoveSourceGone,
    /// Sync could not identify a unique copy to move back for undo.
    UnconfirmedReverse,
    /// The previous Drafts copy could not be removed.
    DraftCleanup,
    /// The folder, or the message in it, never turned up on the server.
    NotFound,
    /// The server has no MOVE. Fell does not copy and expunge instead, so
    /// the change stays in Fell.
    NoMove,
    /// An earlier build copied the message to the target on a server
    /// without MOVE and never expunged the source copy. Fell leaves it.
    Abandoned,
}

const ENTRY_COLS: &str = "id, kind, state, action_id, message_id, source_folder, source_mailbox,
    uidvalidity, uid, target_folder, target_mailbox, placed_uidvalidity, placed_uid,
    attempts, retry_unix, error, generation, sent, reverses,
    flag_value, flag_modseq, flag_base, target_uidvalidity, target_uidnext, move_recovery_end";

impl Store {
    /// Turn write-back on or off for the account (`write_back` in
    /// `account.toml`). Off is the default.
    ///
    /// Turning it off starts a new generation: rows never sent are deleted,
    /// and Fell sends nothing more for any row queued before, even if
    /// write-back comes back on.
    pub fn set_write_back(&self, account_id: &AccountId, enabled: bool) -> Result<()> {
        let tx = crate::store::immediate_transaction(self.connection())?;
        ensure_schema(&tx)?;
        set_write_back_on(&tx, account_id, enabled)?;
        tx.commit()
    }

    /// Whether local moves and flags are queued for the server on this account.
    pub fn write_back(&self, account_id: &AccountId) -> Result<bool> {
        let tx = crate::store::immediate_transaction(self.connection())?;
        ensure_schema(&tx)?;
        let enabled = enabled(&tx, account_id)?;
        tx.commit()?;
        Ok(enabled)
    }

    /// Every outbox row on the account, oldest first.
    pub fn outbox(&self, account_id: &AccountId) -> Result<Vec<OutboxEntry>> {
        let tx = crate::store::immediate_transaction(self.connection())?;
        ensure_schema(&tx)?;
        let entries = select_entries(
            &tx,
            &format!("SELECT {ENTRY_COLS} FROM server_outbox WHERE account_id = ?1 ORDER BY id"),
            account_id,
        )?;
        tx.commit()?;
        Ok(entries)
    }
}

pub(crate) fn set_write_back_on(
    conn: &Connection,
    account_id: &AccountId,
    enabled: bool,
) -> Result<()> {
    let was_on = self::enabled(conn, account_id)?;
    conn.execute(
        "INSERT INTO write_back_accounts (account_id, enabled) VALUES (?1, ?2)
         ON CONFLICT(account_id) DO UPDATE SET enabled = excluded.enabled",
        params![account_id.as_str(), i64::from(enabled)],
    )?;
    if !enabled {
        if was_on {
            conn.execute(
                "UPDATE write_back_accounts SET generation = generation + 1
                 WHERE account_id = ?1",
                params![account_id.as_str()],
            )?;
        }
        conn.execute(
            "DELETE FROM server_outbox WHERE account_id = ?1 AND state = 'pending'",
            params![account_id.as_str()],
        )?;
    }
    Ok(())
}

pub(crate) fn ensure_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(OUTBOX_SCHEMA)?;
    drafts::ensure_schema(conn)?;
    for (table, column, definition) in ADDED_COLUMNS {
        let present: Option<i64> = conn
            .query_row(
                &format!("SELECT 1 FROM pragma_table_info('{table}') WHERE name = ?1"),
                params![column],
                |row| row.get(0),
            )
            .optional()?;
        if present.is_none() {
            conn.execute_batch(&format!(
                "ALTER TABLE {table} ADD COLUMN {column} {definition}"
            ))?;
        }
    }
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_outbox_draft
         ON server_outbox(account_id, draft_id, id) WHERE draft_id IS NOT NULL;",
    )?;
    // Earlier builds of write-back copied, flagged, and expunged on a server
    // without MOVE. A row they left half done is never finished: the
    // source copy stays (maybe flagged `\Deleted`), and it is reported once.
    conn.execute(
        "UPDATE server_outbox
         SET state = 'abandoned', error = 'half-done copy from an earlier build',
             settled_unix = ?1
         WHERE state = 'copied'",
        params![now_unix()],
    )?;
    Ok(())
}

/// The account's write-back generation: rows from any other are never sent.
fn generation(conn: &Connection, account_id: &AccountId) -> Result<i64> {
    let generation: Option<i64> = conn
        .query_row(
            "SELECT generation FROM write_back_accounts WHERE account_id = ?1",
            params![account_id.as_str()],
            |row| row.get(0),
        )
        .optional()?;
    Ok(generation.unwrap_or(0))
}

fn enabled(conn: &Connection, account_id: &AccountId) -> Result<bool> {
    let enabled: Option<i64> = conn
        .query_row(
            "SELECT enabled FROM write_back_accounts WHERE account_id = ?1",
            params![account_id.as_str()],
            |row| row.get(0),
        )
        .optional()?;
    Ok(enabled.is_some_and(|value| value != 0))
}

/// Queue the Bcc-free SMTP bytes, even if the local row could not be saved.
pub(crate) fn queue_append(
    conn: &Connection,
    account: &AccountId,
    id: &crate::MessageId,
    folder: &str,
    mime: &[u8],
) -> Result<()> {
    if enabled(conn, account)? {
        conn.execute(
            "INSERT INTO server_outbox
                 (account_id, kind, state, message_id, source_folder, target_folder,
                  created_unix, generation, append_mime)
             SELECT ?1, 'append_sent', 'pending', ?2, '', ?3, ?4, ?5, ?6
             WHERE NOT EXISTS (SELECT 1 FROM server_outbox
                                WHERE account_id = ?1 AND kind = 'append_sent'
                                  AND message_id = ?2 AND generation = ?5)",
            params![
                account.as_str(),
                id.as_str(),
                folder,
                now_unix(),
                generation(conn, account)?,
                mime
            ],
        )?;
    }
    Ok(())
}

pub(crate) struct PendingAppend {
    /// A draft whose recovery budget ran out has no payload to retransmit.
    pub mime: Option<Vec<u8>>,
    /// The exact mailbox used if a command may already have reached it.
    pub sent_to: Option<String>,
    pub validity: u32,
    pub next: u32,
    pub checks: u32,
}

pub(crate) fn pending_append(store: &Store, entry: &OutboxEntry) -> Result<Option<PendingAppend>> {
    Ok(store
        .connection()
        .query_row(
            "SELECT o.append_mime, CASE WHEN o.sent THEN o.target_mailbox END,
                    o.target_uidvalidity, o.target_uidnext, o.append_checks
         FROM server_outbox o JOIN write_back_accounts a USING (account_id)
         WHERE o.id = ?1 AND o.state = 'pending' AND a.enabled
           AND o.generation = a.generation AND o.retry_unix <= ?2",
            params![entry.id, now_unix()],
            |row| {
                Ok(PendingAppend {
                    mime: row.get(0)?,
                    sent_to: row.get(1)?,
                    validity: row.get(2)?,
                    next: row.get(3)?,
                    checks: row.get(4)?,
                })
            },
        )
        .optional()?)
}

pub(crate) fn mark_append_sent(
    store: &Store,
    entry: &OutboxEntry,
    mailbox: &str,
    bound: Bound,
) -> Result<bool> {
    let tx = crate::store::immediate_transaction(store.connection())?;
    let (validity, next) = bound;
    let changed = tx.execute(
        "UPDATE server_outbox
         SET sent = 1, target_mailbox = ?2, target_uidvalidity = ?3, target_uidnext = ?4,
             append_checks = 0
         WHERE id = ?1 AND state = 'pending' AND generation IN
             (SELECT generation FROM write_back_accounts WHERE account_id = server_outbox.account_id AND enabled)
           AND NOT EXISTS (SELECT 1 FROM compose_send s WHERE s.account_id = server_outbox.account_id
                           AND s.draft_id = server_outbox.draft_id AND s.state = 'sent')
           AND (draft_id IS NULL OR EXISTS (SELECT 1 FROM compose_drafts d
                   WHERE d.account_id = server_outbox.account_id AND d.id = server_outbox.draft_id
                     AND d.discarded_action_id IS NULL))",
        params![entry.id, mailbox, validity, next],
    )?;
    tx.commit()?;
    Ok(changed != 0)
}

pub(crate) fn complete_append(store: &Store, entry: &OutboxEntry) -> Result<bool> {
    let tx = crate::store::immediate_transaction(store.connection())?;
    let changed = complete_append_in(&tx, entry)?;
    tx.commit()?;
    Ok(changed)
}

fn complete_append_in(tx: &Connection, entry: &OutboxEntry) -> Result<bool> {
    let changed = tx.execute(
        "UPDATE server_outbox
         SET state = 'done', append_mime = NULL, error = '', settled_unix = ?2
         WHERE id = ?1 AND state = 'pending'",
        params![entry.id, now_unix()],
    )?;
    Ok(changed != 0)
}

/// Queue the server side of moving `messages` from `from` to `to`, in the
/// transaction that made the local change. Does nothing unless write-back
/// is on for the account.
///
/// Each copy of a message recorded in `from` is one row. When the latest
/// move queued for the message goes into `from` and has not reached the
/// server yet, the copy it will place is not recorded there: one more row,
/// with no UID yet, follows that move. It is found once the move is sent.
pub(crate) fn queue_moves(
    conn: &Connection,
    account_id: &AccountId,
    action_id: i64,
    messages: &[String],
    from: &str,
    to: &str,
    roles: &FolderRoles,
) -> Result<()> {
    ensure_schema(conn)?;
    if roles.is_view(from) || roles.is_view(to) || !enabled(conn, account_id)? {
        return Ok(());
    }
    let target = raw_name(conn, account_id, roles, to)?;
    let source = raw_name(conn, account_id, roles, from)?;
    let now = now_unix();
    let generation = generation(conn, account_id)?;
    for message_id in messages {
        let mut copies = Vec::new();
        for copy in copies_in(conn, account_id, message_id, from)? {
            // An error here rolls back the local change with it.
            if !claimed(conn, account_id, &copy)? {
                copies.push(copy);
            }
        }
        let follows = match inbound(conn, account_id, generation, message_id)? {
            Some((folder, state, placed)) if folder.eq_ignore_ascii_case(from) => match state {
                OutboxState::Pending => true,
                // Sent, but the server did not say where the copy went: a
                // sync pass records it.
                OutboxState::Done => !placed && copies.is_empty(),
                OutboxState::Dropped | OutboxState::Abandoned => false,
            },
            _ => false,
        };
        for (mailbox, uidvalidity, uid) in copies {
            insert(
                conn,
                account_id,
                &NewRow {
                    action_id: Some(action_id),
                    message_id,
                    source_folder: from,
                    source_mailbox: Some(&mailbox),
                    copy: Some((uidvalidity, uid)),
                    target_folder: to,
                    target_mailbox: target.as_deref(),
                    reverses: None,
                },
                now,
            )?;
        }
        if follows {
            insert(
                conn,
                account_id,
                &NewRow {
                    action_id: Some(action_id),
                    message_id,
                    source_folder: from,
                    source_mailbox: source.as_deref(),
                    copy: None,
                    target_folder: to,
                    target_mailbox: target.as_deref(),
                    reverses: None,
                },
                now,
            )?;
        }
    }
    Ok(())
}

/// Undo of `action_id`, in the undo transaction: rows not sent yet go, and
/// each row of this write-back generation the server applied, or may have,
/// gets its reverse move. Only messages whose snapshots remain are undone.
/// An earlier move of a purged message still runs forward, into server Trash.
/// With write-back off, undo is local.
pub(crate) fn undo_action(conn: &Connection, account_id: &AccountId, action_id: i64) -> Result<()> {
    ensure_schema(conn)?;
    let undoable = "EXISTS (
        SELECT 1 FROM local_action_messages s
        WHERE s.action_id = server_outbox.action_id AND s.message_id = server_outbox.message_id
    )";
    // A sent row stays: its MOVE may have reached the server.
    conn.execute(
        &format!(
            "DELETE FROM server_outbox
             WHERE account_id = ?1 AND action_id = ?2 AND kind = 'move' AND state = 'pending' AND sent = 0
               AND {undoable}"
        ),
        params![account_id.as_str(), action_id],
    )?;
    if !enabled(conn, account_id)? {
        return Ok(());
    }
    let generation = generation(conn, account_id)?;
    // `action_id` and `generation` are integers, so they are safe in the
    // SQL text.
    let applied = select_entries(
        conn,
        &format!(
            "SELECT {ENTRY_COLS} FROM server_outbox
             WHERE account_id = ?1 AND action_id = {action_id} AND kind = 'move'
               AND (state IN ('done', 'pending') OR (state = 'dropped' AND sent = 1
                    AND error != 'half-done copy from an earlier build'))
               AND generation = {generation}
               AND {undoable}
             ORDER BY id"
        ),
        account_id,
    )?;
    let now = now_unix();
    for entry in applied {
        if entry.state == OutboxState::Done {
            queue_reverse(conn, account_id, &entry, now)?;
        } else {
            queue_reverse_of_unsettled(conn, account_id, &entry, now)?;
        }
    }
    Ok(())
}

/// Undo of a row sent but not settled. The copy it places is not known
/// yet, so the reverse has no UID: replay takes it from `entry` once that
/// is done (rows for one message go in order), and retires the reverse if
/// `entry` never moved the message.
fn queue_reverse_of_unsettled(
    conn: &Connection,
    account_id: &AccountId,
    entry: &OutboxEntry,
    now: i64,
) -> Result<()> {
    insert(
        conn,
        account_id,
        &NewRow {
            action_id: None,
            message_id: &entry.message_id,
            source_folder: &entry.target_folder,
            source_mailbox: entry.target_mailbox.as_deref(),
            copy: None,
            target_folder: &entry.source_folder,
            target_mailbox: entry.source_mailbox.as_deref(),
            reverses: Some(entry.id),
        },
        now,
    )
}

/// The move that puts `entry`'s message back where it was.
pub(crate) fn queue_reverse(
    conn: &Connection,
    account_id: &AccountId,
    entry: &OutboxEntry,
    now: i64,
) -> Result<()> {
    let copy = entry.placed_uidvalidity.zip(entry.placed_uid);
    insert(
        conn,
        account_id,
        &NewRow {
            action_id: None,
            message_id: &entry.message_id,
            source_folder: &entry.target_folder,
            source_mailbox: entry.target_mailbox.as_deref(),
            copy,
            target_folder: &entry.source_folder,
            target_mailbox: entry.source_mailbox.as_deref(),
            reverses: None,
        },
        now,
    )
}

struct NewRow<'a> {
    action_id: Option<i64>,
    message_id: &'a str,
    source_folder: &'a str,
    source_mailbox: Option<&'a str>,
    copy: Option<(u32, u32)>,
    target_folder: &'a str,
    target_mailbox: Option<&'a str>,
    reverses: Option<i64>,
}

fn insert(conn: &Connection, account_id: &AccountId, row: &NewRow<'_>, now: i64) -> Result<()> {
    conn.execute(
        "INSERT INTO server_outbox
            (account_id, kind, state, action_id, message_id, source_folder, source_mailbox,
             uidvalidity, uid, target_folder, target_mailbox, created_unix, generation,
             reverses)
         VALUES (?1, ?2, 'pending', ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11,
                 (SELECT COALESCE(MAX(generation), 0) FROM write_back_accounts
                  WHERE account_id = ?1),
                 ?12)",
        params![
            account_id.as_str(),
            OutboxKind::Move.as_str(),
            row.action_id,
            row.message_id,
            row.source_folder,
            row.source_mailbox,
            row.copy.map(|(uidvalidity, _)| uidvalidity),
            row.copy.map(|(_, uid)| uid),
            row.target_folder,
            row.target_mailbox,
            now,
            row.reverses.unwrap_or(0),
        ],
    )?;
    Ok(())
}

/// The server's raw name for a folder: its role's, else one a recorded copy
/// came from. `None` leaves it to the server's LIST at replay.
fn raw_name(
    conn: &Connection,
    account_id: &AccountId,
    roles: &FolderRoles,
    folder: &str,
) -> Result<Option<String>> {
    if let Some(found) = roles
        .iter()
        .find(|known| known.name.eq_ignore_ascii_case(folder))
    {
        return Ok(Some(found.raw.clone()));
    }
    let mailbox: Option<String> = conn
        .query_row(
            "SELECT mailbox FROM message_copies
             WHERE account_id = ?1 AND folder = ?2 COLLATE NOCASE LIMIT 1",
            params![account_id.as_str(), folder],
            |row| row.get(0),
        )
        .optional()?;
    Ok(mailbox.and_then(|escaped| crate::imap::unescape_mailbox(&escaped)))
}

/// `(raw mailbox, UIDVALIDITY, UID)` of each copy of the message in `folder`.
fn copies_in(
    conn: &Connection,
    account_id: &AccountId,
    message_id: &str,
    folder: &str,
) -> Result<Vec<(String, u32, u32)>> {
    let mut stmt = conn.prepare_cached(
        "SELECT mailbox, uidvalidity, uid FROM message_copies
         WHERE account_id = ?1 AND message_id = ?2 AND folder = ?3 COLLATE NOCASE
         ORDER BY mailbox, uidvalidity, uid",
    )?;
    let rows = stmt.query_map(params![account_id.as_str(), message_id, folder], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, u32>(1)?,
            row.get::<_, u32>(2)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (escaped, uidvalidity, uid) = row?;
        if let Some(mailbox) = crate::imap::unescape_mailbox(&escaped) {
            out.push((mailbox, uidvalidity, uid));
        }
    }
    Ok(out)
}

/// An unfinished row already moves this copy, so it will not be there.
fn claimed(conn: &Connection, account_id: &AccountId, copy: &(String, u32, u32)) -> Result<bool> {
    let found: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM server_outbox
             WHERE account_id = ?1 AND state = 'pending'
               AND kind = 'move' AND source_mailbox = ?2 AND uidvalidity = ?3 AND uid = ?4
             LIMIT 1",
            params![account_id.as_str(), copy.0, copy.1, copy.2],
            |row| row.get(0),
        )
        .optional()?;
    Ok(found.is_some())
}

/// The latest live row for the message in this generation: its target
/// folder, its state, and whether the server said where it put the copy.
fn inbound(
    conn: &Connection,
    account_id: &AccountId,
    generation: i64,
    message_id: &str,
) -> Result<Option<(String, OutboxState, bool)>> {
    let found: Option<(String, String, bool)> = conn
        .query_row(
            "SELECT target_folder, state, placed_uid IS NOT NULL FROM server_outbox
             WHERE account_id = ?1 AND message_id = ?2 AND generation = ?3 AND kind = 'move'
               AND state IN ('pending', 'done')
             ORDER BY id DESC LIMIT 1",
            params![account_id.as_str(), message_id, generation],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    found
        .map(|(folder, state, placed)| Ok((folder, OutboxState::parse(&state)?, placed)))
        .transpose()
}

fn select_entries(
    conn: &Connection,
    sql: &str,
    account_id: &AccountId,
) -> Result<Vec<OutboxEntry>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params![account_id.as_str()], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Option<i64>>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, Option<String>>(6)?,
            row.get::<_, Option<u32>>(7)?,
            row.get::<_, Option<u32>>(8)?,
            row.get::<_, String>(9)?,
            row.get::<_, Option<String>>(10)?,
            (
                row.get::<_, Option<u32>>(11)?,
                row.get::<_, Option<u32>>(12)?,
                row.get::<_, u32>(13)?,
                row.get::<_, i64>(14)?,
                row.get::<_, String>(15)?,
                row.get::<_, i64>(16)?,
                row.get::<_, bool>(17)?,
                row.get::<_, i64>(18)?,
                row.get::<_, Option<bool>>(19)?,
                row.get::<_, Option<[u8; 8]>>(20)?.map(u64::from_be_bytes),
                row.get::<_, Option<bool>>(21)?,
                (row.get::<_, u32>(22)?, row.get::<_, u32>(23)?),
                row.get::<_, Option<u32>>(24)?,
            ),
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (
            id,
            kind,
            state,
            action_id,
            message_id,
            source_folder,
            source_mailbox,
            uidvalidity,
            uid,
            target_folder,
            target_mailbox,
            (
                placed_uidvalidity,
                placed_uid,
                attempts,
                retry_unix,
                error,
                generation,
                sent,
                reverses,
                flag_value,
                flag_modseq,
                flag_base,
                target_bound,
                move_recovery_end,
            ),
        ) = row?;
        out.push(OutboxEntry {
            id,
            kind: OutboxKind::parse(&kind)?,
            state: OutboxState::parse(&state)?,
            action_id,
            message_id,
            source_folder,
            source_mailbox,
            uidvalidity,
            uid,
            target_folder,
            target_mailbox,
            placed_uidvalidity,
            placed_uid,
            attempts,
            retry_unix,
            error,
            generation,
            sent,
            target_bound: (target_bound.1 > 0).then_some(target_bound),
            move_recovery_end,
            reverses: (reverses != 0).then_some(reverses),
            flag_value,
            flag_modseq,
            flag_base,
        });
    }
    Ok(out)
}

// Replay's side. Each call is its own short write transaction, so the
// window's actions never wait on the network.

/// Rows not yet applied in this generation, oldest first, when write-back
/// is on. Settled rows past [`KEEP_SETTLED_SECS`] are pruned on the way.
pub(crate) fn unfinished(store: &Store, account_id: &AccountId) -> Result<Vec<OutboxEntry>> {
    let tx = crate::store::immediate_transaction(store.connection())?;
    ensure_schema(&tx)?;
    if !enabled(&tx, account_id)? {
        tx.commit()?;
        return Ok(Vec::new());
    }
    tx.execute(
        "DELETE FROM server_outbox AS settled
         WHERE account_id = ?1 AND state IN ('done', 'dropped') AND settled_unix < ?2
           AND NOT EXISTS (SELECT 1 FROM server_outbox pending
                           WHERE pending.reverses = settled.id AND pending.state = 'pending')
           AND NOT (kind = 'append_draft' AND state = 'dropped' AND append_checks > 0 AND generation = ?3)",
        params![account_id.as_str(), now_unix() - KEEP_SETTLED_SECS, generation(&tx, account_id)?],
    )?;
    let generation = generation(&tx, account_id)?;
    labels::bind_keyword_copies(&tx, account_id, generation)?;
    let entries = select_entries(
        &tx,
        &format!(
            "SELECT {ENTRY_COLS} FROM server_outbox
             WHERE account_id = ?1 AND state = 'pending'
               AND generation = {generation}
             ORDER BY COALESCE(keyword_parent, id), id"
        ),
        account_id,
    )?;
    tx.commit()?;
    Ok(entries)
}

/// Whether any row is due now, without resolving the password or opening a
/// connection when none is.
pub(crate) fn any_due(store: &Store, account_id: &AccountId, append_sent: bool) -> Result<bool> {
    let tx = crate::store::immediate_transaction(store.connection())?;
    ensure_schema(&tx)?;
    let due: Option<i64> = tx
        .query_row(
            "SELECT 1 FROM server_outbox
             WHERE account_id = ?1 AND state = 'pending' AND retry_unix <= ?2
               AND (?3 OR kind != 'append_sent')
               AND generation = (SELECT COALESCE(MAX(generation), 0)
                                 FROM write_back_accounts WHERE account_id = ?1)
             LIMIT 1",
            params![account_id.as_str(), now_unix(), append_sent],
            |row| row.get(0),
        )
        .optional()?;
    tx.commit()?;
    Ok(due.is_some())
}

/// The newest copy of the message in `mailbox` (raw name) under
/// `uidvalidity` that no unfinished row moves already, for a row queued
/// before its copy reached the server. Another copy that was there before
/// has its own row, so it is not taken twice.
pub(crate) fn find_copy(
    store: &Store,
    account_id: &AccountId,
    message_id: &str,
    mailbox: &str,
    uidvalidity: u32,
) -> Result<Option<u32>> {
    store
        .connection()
        .query_row(
            "SELECT MAX(c.uid) FROM message_copies c
             WHERE c.account_id = ?1 AND c.message_id = ?2 AND c.mailbox = ?3
               AND c.uidvalidity = ?4
               AND NOT EXISTS (
                   SELECT 1 FROM server_outbox o
                   WHERE o.account_id = ?1 AND o.state = 'pending'
                     AND o.source_mailbox = ?5 AND o.uidvalidity = c.uidvalidity
                     AND o.uid = c.uid)",
            params![
                account_id.as_str(),
                message_id,
                crate::imap::escape_mailbox(mailbox),
                uidvalidity,
                mailbox,
            ],
            |row| row.get(0),
        )
        .map_err(Into::into)
}

/// An unfinished row moves the copy at `uid` in `mailbox` (raw name), so
/// it is not the one a move just placed there.
pub(crate) fn is_claimed(
    store: &Store,
    account_id: &AccountId,
    mailbox: &str,
    uidvalidity: u32,
    uid: u32,
) -> Result<bool> {
    claimed(
        store.connection(),
        account_id,
        &(mailbox.to_string(), uidvalidity, uid),
    )
}

/// A target's `(UIDVALIDITY, UIDNEXT)` before a move into it.
pub(crate) type Bound = (u32, u32);

/// Claim unsent moves and record the exact source copies and destination
/// bound before sending. A later pass can recover even a move whose source
/// UID was initially deferred and has since disappeared from the source.
pub(crate) fn mark_moves_sent(
    store: &Store,
    account_id: &AccountId,
    moves: &[(i64, u32)],
    (source, validity): (&str, u32),
    target: &str,
    bound: Bound,
) -> Result<Vec<i64>> {
    let tx = crate::store::immediate_transaction(store.connection())?;
    ensure_schema(&tx)?;
    let mut marked = Vec::new();
    if enabled(&tx, account_id)? {
        let generation = generation(&tx, account_id)?;
        for &(id, uid) in moves {
            let changed = tx.execute(
                "UPDATE server_outbox
                 SET sent = 1, source_mailbox = ?2, uidvalidity = ?3, uid = ?4,
                     target_mailbox = ?5, target_uidvalidity = ?6, target_uidnext = ?7,
                     move_recovery_end = NULL
                 WHERE id = ?1 AND account_id = ?8 AND state = 'pending'
                   AND generation = ?9 AND sent = 0",
                params![
                    id,
                    source,
                    validity,
                    uid,
                    target,
                    bound.0,
                    bound.1,
                    account_id.as_str(),
                    generation
                ],
            )?;
            if changed != 0 {
                marked.push(id);
            }
        }
    }
    tx.commit()?;
    Ok(marked)
}

/// Save the exclusive UID bound before further recovery commands can fail.
pub(crate) fn record_move_recovery_end(store: &Store, id: i64, end: u32) -> Result<()> {
    store.connection().execute(
        "UPDATE server_outbox SET move_recovery_end = ?2
         WHERE id = ?1 AND state = 'pending' AND sent = 1 AND move_recovery_end IS NOT ?2",
        params![id, end],
    )?;
    Ok(())
}

/// Record that a label write for these rows is about to go out. `target` is the
/// target's `(UIDVALIDITY, UIDNEXT)` just now; a row not sent before keeps
/// it, so a later look for its copy there trusts only UIDs from it up.
///
/// Only rows still pending in the current generation, with write-back on,
/// are marked: one that undo removed, or that turning write-back off
/// retired, since the pass loaded it is not sent. Returns each marked id
/// with the bound it keeps (`None` when unknown, from an earlier build).
pub(crate) fn mark_sent(
    store: &Store,
    account_id: &AccountId,
    ids: &[i64],
    target: Bound,
) -> Result<Vec<(i64, Option<Bound>)>> {
    let tx = crate::store::immediate_transaction(store.connection())?;
    ensure_schema(&tx)?;
    let mut marked = Vec::new();
    if enabled(&tx, account_id)? {
        let generation = generation(&tx, account_id)?;
        for id in ids {
            // SET reads the old `sent`, so only a first send records the bound.
            let changed = tx.execute(
                "UPDATE server_outbox
                 SET sent = 1,
                     target_uidvalidity = CASE WHEN sent = 0 THEN ?2 ELSE target_uidvalidity END,
                     target_uidnext = CASE WHEN sent = 0 THEN ?3 ELSE target_uidnext END
                 WHERE id = ?1 AND account_id = ?4 AND state = 'pending' AND generation = ?5",
                params![id, target.0, target.1, account_id.as_str(), generation],
            )?;
            if changed == 0 {
                continue;
            }
            let bound: Bound = tx.query_row(
                "SELECT target_uidvalidity, target_uidnext FROM server_outbox WHERE id = ?1",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            marked.push((*id, (bound.1 > 0).then_some(bound)));
        }
    }
    tx.commit()?;
    Ok(marked)
}

/// `report` as the status line should have it. Two notices are said only once:
///
/// - "no MOVE": recorded as told when it appears, and left out after that;
/// - a half-done copy from an earlier build ([`take_abandoned`]): taken only
///   by a report with nothing else to say, so it waits for the next pass.
pub(crate) fn report_for_status(
    store: &Store,
    account_id: &AccountId,
    mut report: ReplayReport,
) -> Result<ReplayReport> {
    let no_move = report
        .dropped
        .iter()
        .any(|dropped| dropped.reason == DropReason::NoMove);
    if no_move && note_move_missing(store, account_id, true)? {
        report
            .dropped
            .retain(|dropped| dropped.reason != DropReason::NoMove);
    }
    if report.dropped.is_empty() && !report.flag_recovery_delayed && !report.sent_append_delayed {
        report.dropped = take_abandoned(store, account_id)?;
    }
    Ok(report)
}

/// Whether the user was already told this account's server has no MOVE,
/// after recording that it has (`missing`) or has not. The notice is shown
/// once, not on every archive.
pub(crate) fn note_move_missing(
    store: &Store,
    account_id: &AccountId,
    missing: bool,
) -> Result<bool> {
    let tx = crate::store::immediate_transaction(store.connection())?;
    ensure_schema(&tx)?;
    let changed = tx.execute(
        "UPDATE write_back_accounts SET move_missing = ?2
         WHERE account_id = ?1 AND move_missing != ?2",
        params![account_id.as_str(), i64::from(missing)],
    )?;
    tx.commit()?;
    Ok(missing && changed == 0)
}

/// Half-done copies an earlier build left (see [`OutboxState::Abandoned`]),
/// each reported once. Opens no connection, so it runs with write-back off
/// too.
pub(crate) fn take_abandoned(store: &Store, account_id: &AccountId) -> Result<Vec<DroppedWrite>> {
    let tx = crate::store::immediate_transaction(store.connection())?;
    ensure_schema(&tx)?;
    let abandoned = select_entries(
        &tx,
        &format!(
            "SELECT {ENTRY_COLS} FROM server_outbox
             WHERE account_id = ?1 AND state = 'abandoned' ORDER BY id"
        ),
        account_id,
    )?;
    tx.execute(
        "UPDATE server_outbox SET state = 'dropped', sent = 0
         WHERE account_id = ?1 AND state = 'abandoned'",
        params![account_id.as_str()],
    )?;
    tx.commit()?;
    Ok(abandoned
        .into_iter()
        .map(|entry| DroppedWrite {
            kind: entry.kind,
            source_folder: entry.source_folder,
            target_folder: entry.target_folder,
            reason: DropReason::Abandoned,
        })
        .collect())
}

/// What replay learned about one row.
pub(crate) enum Outcome<'a> {
    /// The server applied it, and `placed` is the new copy: from COPYUID,
    /// or found in the target. A move whose new copy is not known is not
    /// done: it stays [`Outcome::Retry`] until it is dropped, so the source
    /// copy stays in `message_copies` until a new one replaces it.
    Done { placed: (u32, u32) },
    /// Try again at `retry_unix`. `counts` is false for a dropped connection,
    /// which says nothing about the row. `refused`: a NO or BAD reply or
    /// recovery confirmed the MOVE did not land. The next try can send again.
    Retry {
        error: &'a str,
        retry_unix: i64,
        counts: bool,
        refused: bool,
        append_check: bool,
    },
    Dropped {
        error: &'a str,
        draft_cleanup: bool,
        draft_lookup: Option<(u32, u32)>,
    },
}

/// What [`settle`] did with a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Settled {
    /// Recorded as asked.
    Recorded,
    /// A superseded flag STORE may already have landed. Its replacement
    /// waits for a complete observation of that attempt.
    Recovering,
    /// A row that no longer needs another attempt or a notice, such as an
    /// undone label, a superseded flag or a refused move that undo reversed.
    Retired,
    /// The row is from an earlier write-back generation (write-back was
    /// turned off while it was on the wire) and is left as it is. The pass
    /// must send nothing more.
    Stale,
}

/// Record `outcome` for `entry`, whose copy was resolved to
/// `(source_mailbox, uidvalidity, uid)` and target to `target_mailbox`.
///
/// A sent move also moves the copy in `message_copies`, so a later action
/// finds the message where the server now has it. If the row was removed
/// while it was being sent, the server acted anyway, so the reverse is
/// queued here.
pub(crate) fn settle(
    store: &Store,
    account_id: &AccountId,
    entry: &OutboxEntry,
    resolved: Option<(&str, u32, u32)>,
    target_mailbox: Option<&str>,
    outcome: Outcome<'_>,
) -> Result<Settled> {
    let tx = crate::store::immediate_transaction(store.connection())?;
    ensure_schema(&tx)?;
    let now = now_unix();
    let live: Option<(i64, bool, bool, bool)> = tx
        .query_row(
            "SELECT generation, flag_superseded, sent, state = 'pending' FROM server_outbox
             WHERE id = ?1 AND (kind = 'move' OR state = 'pending')",
            params![entry.id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    let current = enabled(&tx, account_id)?
        .then(|| generation(&tx, account_id))
        .transpose()?;
    if Some(live.map_or(entry.generation, |(generation, ..)| generation)) != current {
        tx.commit()?;
        return Ok(Settled::Stale);
    }
    if matches!(live, Some((_, _, _, false))) || (live.is_none() && entry.kind != OutboxKind::Move)
    {
        tx.commit()?;
        return Ok(Settled::Retired);
    }
    let recovering = entry.kind.flag_name().is_some()
        && matches!(live, Some((_, true, true, _)))
        && matches!(outcome, Outcome::Retry { refused: false, .. });
    if entry.kind.flag_name().is_some() && matches!(live, Some((_, true, _, _))) && !recovering {
        tx.execute(
            "UPDATE server_outbox SET state = 'dropped', error = 'superseded', settled_unix = ?2
             WHERE id = ?1 AND state = 'pending'",
            params![entry.id, now],
        )?;
        tx.commit()?;
        return Ok(Settled::Retired);
    }
    let placed = match outcome {
        Outcome::Done { placed } => placed,
        Outcome::Retry {
            error,
            retry_unix,
            counts,
            refused,
            append_check,
        } => {
            // Undone while it was on the wire, and now refused: the server
            // never moved it, so there is nothing to move back.
            if refused && live.is_some() && retire_reversed(&tx, account_id, entry.id, now)? {
                tx.commit()?;
                return Ok(Settled::Retired);
            }
            tx.execute(
                "UPDATE server_outbox
                 SET attempts = attempts + ?2, retry_unix = ?3, error = ?4,
                     sent = CASE WHEN ?5 THEN 0 ELSE sent END,
                     append_checks = append_checks + ?6
                 WHERE id = ?1",
                params![
                    entry.id,
                    i64::from(counts),
                    retry_unix,
                    error,
                    refused,
                    append_check
                ],
            )?;
            tx.commit()?;
            return Ok(if recovering {
                Settled::Recovering
            } else {
                Settled::Recorded
            });
        }
        Outcome::Dropped {
            error,
            draft_cleanup,
            draft_lookup,
        } => {
            let settled = match draft_lookup {
                Some(lookup) => {
                    drafts::recover_for_waiting_save(&tx, account_id, entry, lookup, error)?
                }
                None => None,
            };
            if settled == Some(Settled::Recovering) {
                tx.commit()?;
                return Ok(Settled::Recovering);
            }
            if draft_cleanup {
                drafts::abandon_cleanup(store, &tx, account_id, entry)?;
            }
            tx.execute(
                "UPDATE server_outbox
                 SET state = 'dropped', append_mime = NULL, error = ?2, settled_unix = ?3,
                     append_checks = CASE WHEN ?4 THEN 0 ELSE append_checks END
                 WHERE id = ?1",
                params![entry.id, error, now, draft_cleanup],
            )?;
            tx.commit()?;
            return Ok(settled.unwrap_or(Settled::Recorded));
        }
    };
    let (source_mailbox, uidvalidity, uid) =
        resolved.ok_or_else(|| Error::Action("a sent move needs its copy".into()))?;
    // The row is gone, but the server has acted on it. Undo keeps a sent
    // row (and queues its reverse), so this is only a guard: put the row
    // back, no longer tied to the action, then reverse it.
    let undone_meanwhile = live.is_none() && entry.state == OutboxState::Pending;
    if undone_meanwhile {
        tx.execute(
            "INSERT INTO server_outbox
                (id, account_id, kind, state, action_id, message_id, source_folder,
                 target_folder, created_unix, generation, sent)
             VALUES (?1, ?2, ?3, 'pending', NULL, ?4, ?5, ?6, ?7, ?8, 1)",
            params![
                entry.id,
                account_id.as_str(),
                entry.kind.as_str(),
                entry.message_id,
                entry.source_folder,
                entry.target_folder,
                now,
                entry.generation,
            ],
        )?;
    }
    let mut sent = entry.clone();
    sent.state = OutboxState::Done;
    sent.source_mailbox = Some(source_mailbox.to_string());
    sent.uidvalidity = Some(uidvalidity);
    sent.uid = Some(uid);
    sent.target_mailbox = target_mailbox.map(str::to_string);
    sent.placed_uidvalidity = Some(placed.0);
    sent.placed_uid = Some(placed.1);
    tx.execute(
        "UPDATE server_outbox
         SET state = ?2, source_mailbox = ?3, uidvalidity = ?4, uid = ?5, target_mailbox = ?6,
             placed_uidvalidity = ?7, placed_uid = ?8, error = '', settled_unix = ?9
         WHERE id = ?1",
        params![
            entry.id,
            OutboxState::Done.as_str(),
            sent.source_mailbox,
            sent.uidvalidity,
            sent.uid,
            sent.target_mailbox,
            sent.placed_uidvalidity,
            sent.placed_uid,
            now,
        ],
    )?;
    move_copy(&tx, account_id, &sent)?;
    if undone_meanwhile {
        queue_reverse(&tx, account_id, &sent, now)?;
    }
    tx.commit()?;
    Ok(Settled::Recorded)
}

/// If undo queued a reverse of row `id`, drop both: `id` never reached the
/// server. Returns whether there was one.
fn retire_reversed(conn: &Connection, account_id: &AccountId, id: i64, now: i64) -> Result<bool> {
    let reverses = conn.execute(
        "UPDATE server_outbox
         SET state = 'dropped', error = 'undone; the server did not move it', settled_unix = ?3
         WHERE account_id = ?1 AND reverses = ?2 AND state = 'pending'",
        params![account_id.as_str(), id, now],
    )?;
    if reverses == 0 {
        return Ok(false);
    }
    conn.execute(
        "UPDATE server_outbox
         SET state = 'dropped', sent = 0, error = 'undone; the server did not move it', settled_unix = ?2
         WHERE id = ?1",
        params![id, now],
    )?;
    Ok(true)
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum MoveSource {
    Present,
    Gone,
    Unknown,
}

const UNCONFIRMED_SOURCE_GONE: &str = "Destination UID unconfirmed. The source copy is gone";

/// Settle the uncertain write and its local filing in one transaction.
pub(crate) fn give_up_move(
    store: &Store,
    account: &AccountId,
    entry: &OutboxEntry,
    observed: MoveSource,
    source: Option<(&str, (u32, u32))>,
) -> Result<Option<DropReason>> {
    let tx = crate::store::immediate_transaction(store.connection())?;
    let now = now_unix();
    let error = match observed {
        MoveSource::Present => "Destination UID unconfirmed. The source copy is still present",
        MoveSource::Gone => UNCONFIRMED_SOURCE_GONE,
        MoveSource::Unknown => "Destination and source UIDs unconfirmed",
    };
    let changed = tx.execute(
        "UPDATE server_outbox
         SET state = 'dropped', error = ?2, settled_unix = ?3, sent = ?4
         WHERE id = ?1 AND state = 'pending' AND sent = 1
           AND generation IN (SELECT generation FROM write_back_accounts
                              WHERE account_id = server_outbox.account_id AND enabled)",
        params![entry.id, error, now, observed != MoveSource::Present],
    )?;
    let reason = if changed == 0 {
        None
    } else if observed == MoveSource::Present {
        if retire_reversed(&tx, account, entry.id, now)? {
            None
        } else if let Some((mailbox, (validity, uid))) = source
            && crate::store::copies::restore_move_source(
                &tx, account, entry, mailbox, validity, uid,
            )?
        {
            Some(DropReason::MoveRestored)
        } else {
            Some(DropReason::UnconfirmedMove)
        }
    } else if observed == MoveSource::Gone {
        Some(DropReason::MoveSourceGone)
    } else {
        Some(DropReason::UnconfirmedMove)
    };
    tx.commit()?;
    Ok(reason)
}

/// Where the row a reverse follows stands now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reversed {
    /// Still unsettled: the reverse waits behind it.
    Waiting,
    /// The server applied it and placed the copy at `(UIDVALIDITY, UID)`
    /// in the reverse's source mailbox.
    Placed(u32, u32),
    /// A dropped sent move may have landed. Only sync can supply the copy.
    Unconfirmed {
        /// Check this original UID first if it wasn't confirmed gone.
        source: Option<(u32, u32)>,
        placed: Option<(u32, u32)>,
    },
    /// The source copy stayed on the server: nothing to move back.
    Gone,
}

/// The state of row `id`, which undo's reverse row follows.
pub(crate) fn reversed(store: &Store, account: &AccountId, id: i64) -> Result<Reversed> {
    let entries = select_entries(
        store.connection(),
        &format!("SELECT {ENTRY_COLS} FROM server_outbox WHERE account_id = ?1 AND id = {id}"),
        account,
    )?;
    let Some(entry) = entries.first() else {
        return Ok(Reversed::Gone);
    };
    Ok(
        match (entry.state, entry.placed_uidvalidity.zip(entry.placed_uid)) {
            (OutboxState::Pending, _) => Reversed::Waiting,
            (OutboxState::Done, Some((uidvalidity, uid))) => Reversed::Placed(uidvalidity, uid),
            (OutboxState::Dropped, _)
                if entry.sent && entry.error != "half-done copy from an earlier build" =>
            {
                synced_reversal(store, account, entry)?
            }
            _ => Reversed::Gone,
        },
    )
}

fn synced_reversal(store: &Store, account: &AccountId, entry: &OutboxEntry) -> Result<Reversed> {
    let source = if entry.error == UNCONFIRMED_SOURCE_GONE {
        None
    } else {
        entry.uidvalidity.zip(entry.uid)
    };
    let placed = if let Some((mailbox, (validity, first))) =
        entry.target_mailbox.as_deref().zip(entry.target_bound)
    {
        let end = entry
            .move_recovery_end
            .map(i64::from)
            .unwrap_or(i64::from(first) + crate::imap::RECOVERY_UID_LIMIT as i64);
        let uid: Option<u32> = store.connection().query_row(
            "SELECT CASE WHEN COUNT(*) = 1 THEN MAX(c.uid) END FROM message_copies c
             WHERE c.account_id = ?1 AND c.message_id = ?2 AND c.mailbox = ?3
               AND c.uidvalidity = ?4 AND c.uid >= ?5 AND c.uid < ?6
               AND NOT EXISTS (SELECT 1 FROM server_outbox o
                               WHERE o.account_id = c.account_id AND o.kind = 'move'
                                 AND ((o.state = 'pending' AND o.source_mailbox = ?7
                                       AND o.uidvalidity = c.uidvalidity AND o.uid = c.uid)
                                   OR (o.target_mailbox = ?7 AND o.placed_uidvalidity = c.uidvalidity
                                       AND o.placed_uid = c.uid)))",
            params![
                account.as_str(),
                entry.message_id,
                crate::imap::escape_mailbox(mailbox),
                validity,
                first,
                end,
                mailbox,
            ],
            |row| row.get(0),
        )?;
        uid.map(|uid| (validity, uid))
    } else {
        None
    };
    Ok(Reversed::Unconfirmed { source, placed })
}

/// The server moved `sent`'s copy: record it where it went. The source copy
/// goes only together with a known new one, so the message never loses
/// every copy it has on record. Its folder is queued the way sync queues a
/// copy it sees go, so the next pass takes the folder off a row that still
/// lists it (a pass may have added it while an undo was on its way).
fn move_copy(conn: &Connection, account_id: &AccountId, sent: &OutboxEntry) -> Result<()> {
    let (
        Some(source),
        Some(uidvalidity),
        Some(uid),
        Some(target),
        Some(placed_uidvalidity),
        Some(placed_uid),
    ) = (
        &sent.source_mailbox,
        sent.uidvalidity,
        sent.uid,
        &sent.target_mailbox,
        sent.placed_uidvalidity,
        sent.placed_uid,
    )
    else {
        return Ok(());
    };
    let copy = params![
        account_id.as_str(),
        crate::imap::escape_mailbox(source),
        uidvalidity,
        uid
    ];
    let (seen, flagged, keywords): (Option<bool>, Option<bool>, String) = conn
        .query_row(
            "SELECT seen, flagged, keywords FROM message_copies
             WHERE account_id = ?1 AND mailbox = ?2 AND uidvalidity = ?3 AND uid = ?4",
            copy,
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?
        .unwrap_or_default();
    conn.execute(
        "INSERT OR IGNORE INTO copy_orphans (account_id, message_id, folder)
         SELECT account_id, message_id, folder FROM message_copies
         WHERE account_id = ?1 AND mailbox = ?2 AND uidvalidity = ?3 AND uid = ?4",
        copy,
    )?;
    conn.execute(
        "DELETE FROM message_copies
         WHERE account_id = ?1 AND mailbox = ?2 AND uidvalidity = ?3 AND uid = ?4",
        copy,
    )?;
    let row_exists: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM messages WHERE account_id = ?1 AND message_id = ?2",
            params![account_id.as_str(), sent.message_id],
            |row| row.get(0),
        )
        .optional()?;
    // A queued chain can still carry a purged message to server Trash. Keep
    // the placed UID for the next move even when the local row is gone.
    if row_exists.is_none()
        && !crate::actions::folder_was_purged(
            conn,
            account_id,
            &sent.message_id,
            &sent.target_folder,
        )?
    {
        return Ok(());
    }
    // MOVE keeps the flags. The destination's MODSEQ comes from its mailbox.
    conn.execute(
        "INSERT INTO message_copies (account_id, mailbox, uidvalidity, uid, message_id, folder, seen, flagged, keywords)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(account_id, mailbox, uidvalidity, uid) DO NOTHING",
        params![
            account_id.as_str(),
            crate::imap::escape_mailbox(target),
            placed_uidvalidity,
            placed_uid,
            sent.message_id,
            crate::imap::folder_name(target),
            seen,
            flagged,
            keywords,
        ],
    )?;
    conn.execute(
        "UPDATE server_outbox
         SET source_mailbox = ?6, uidvalidity = ?7, uid = ?8, source_folder = ?9
         WHERE account_id = ?1 AND message_id = ?2 AND source_mailbox = ?3
           AND uidvalidity = ?4 AND uid = ?5 AND kind = 'keyword' AND state IN ('pending', 'done')",
        params![
            account_id.as_str(),
            sent.message_id,
            source,
            uidvalidity,
            uid,
            target,
            placed_uidvalidity,
            placed_uid,
            sent.target_folder
        ],
    )?;
    flags::follow_move(conn, account_id, sent)?;
    Ok(())
}

pub(crate) fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}
