//! Server actions waiting to be sent: the durable write-back outbox.
//!
//! Archive and move change the local store at once. When the account has
//! `write_back = true` in `account.toml`, the same [`WriteTx`] also writes
//! one outbox row per server copy the change moved (from `message_copies`),
//! so the window stays instant and a queued action survives a restart. The
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
//! already applied stay, and undo queues the reverse move for each.
//!
//! Moves go out only as `UID MOVE` (RFC 6851). A server without MOVE keeps
//! the change in Fell: Fell never copies, flags `\Deleted`, or expunges.
//!
//! Turning write-back off starts a new generation, and rows not sent yet
//! are deleted. No row from an earlier generation is ever sent or
//! reversed, even when write-back is turned on again or a command on the
//! wire when it went off is answered late.
//!
//! Only moves exist so far. [`OutboxKind`] names what a row asks for, and
//! replay dispatches on it, so flags (#104), delete to Trash (#105), labels
//! (#106), and Drafts or Sent `APPEND` (#116, #117) add a kind each.
//!
//! [`WriteTx`]: crate::store::WriteTx

use rusqlite::{Connection, OptionalExtension, params};

use crate::account::AccountId;
use crate::folders::FolderRoles;
use crate::store::Store;
use crate::{Error, Result};

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
    -- The local_actions row undo reverses; NULL for undo's own reverse moves.
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
    -- 1 once a MOVE for the row may have reached the server.
    sent INTEGER NOT NULL DEFAULT 0,
    -- The target's UIDVALIDITY and UIDNEXT just before the first MOVE was
    -- sent: a copy found there later counts only at or above this UIDNEXT.
    -- 0 when unknown.
    target_uidvalidity INTEGER NOT NULL DEFAULT 0,
    target_uidnext INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_server_outbox_state ON server_outbox(account_id, state, id);
CREATE INDEX IF NOT EXISTS idx_server_outbox_action ON server_outbox(action_id);
CREATE INDEX IF NOT EXISTS idx_server_outbox_message ON server_outbox(account_id, message_id, id);
";

/// Columns added after the tables first shipped on a branch build.
const ADDED_COLUMNS: [(&str, &str); 6] = [
    ("write_back_accounts", "generation"),
    ("write_back_accounts", "move_missing"),
    ("server_outbox", "generation"),
    ("server_outbox", "sent"),
    ("server_outbox", "target_uidvalidity"),
    ("server_outbox", "target_uidnext"),
];

/// Sent and dropped rows are kept this long, so undo can still reverse a
/// move the server applied.
const KEEP_SETTLED_SECS: i64 = 30 * 24 * 60 * 60;

/// What an outbox row asks the server to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutboxKind {
    /// Move one copy from `source` to `target` (archive and move).
    Move,
}

impl OutboxKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Move => "move",
        }
    }

    fn parse(raw: &str) -> Result<Self> {
        match raw {
            "move" => Ok(Self::Move),
            other => Err(Error::Action(format!("unknown outbox action {other}"))),
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

/// One server copy an action asked to move.
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
    /// A MOVE for the row may have reached the server.
    pub sent: bool,
}

/// What one replay pass did ([`crate::AccountConfig::replay_outbox`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReplayReport {
    /// Rows the server applied.
    pub sent: usize,
    /// Rows still queued: refused for now, waiting on an earlier row, or
    /// not due yet.
    pub waiting: usize,
    /// Rows given up on. The local change stays; the server never got it.
    pub dropped: Vec<DroppedMove>,
    /// The connection failed part way; the rest waits for the next pass.
    pub interrupted: Option<String>,
}

/// A move the server will not get.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DroppedMove {
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
    attempts, retry_unix, error, generation, sent";

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
        let was_on = self::enabled(&tx, account_id)?;
        tx.execute(
            "INSERT INTO write_back_accounts (account_id, enabled) VALUES (?1, ?2)
             ON CONFLICT(account_id) DO UPDATE SET enabled = excluded.enabled",
            params![account_id.as_str(), i64::from(enabled)],
        )?;
        if !enabled {
            if was_on {
                tx.execute(
                    "UPDATE write_back_accounts SET generation = generation + 1
                     WHERE account_id = ?1",
                    params![account_id.as_str()],
                )?;
            }
            tx.execute(
                "DELETE FROM server_outbox WHERE account_id = ?1 AND state = 'pending'",
                params![account_id.as_str()],
            )?;
        }
        tx.commit()
    }

    /// Whether archive and move are queued for the server on this account.
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

pub(crate) fn ensure_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(OUTBOX_SCHEMA)?;
    for (table, column) in ADDED_COLUMNS {
        let present: Option<i64> = conn
            .query_row(
                &format!("SELECT 1 FROM pragma_table_info('{table}') WHERE name = ?1"),
                params![column],
                |row| row.get(0),
            )
            .optional()?;
        if present.is_none() {
            conn.execute_batch(&format!(
                "ALTER TABLE {table} ADD COLUMN {column} INTEGER NOT NULL DEFAULT 0"
            ))?;
        }
    }
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
    if !enabled(conn, account_id)? {
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
                },
                now,
            )?;
        }
    }
    Ok(())
}

/// Undo of `action_id`, in the undo transaction: rows not sent yet go, and
/// each row the server applied in this write-back generation gets its
/// reverse move. With write-back off, undo is local.
pub(crate) fn undo_action(conn: &Connection, account_id: &AccountId, action_id: i64) -> Result<()> {
    ensure_schema(conn)?;
    conn.execute(
        "DELETE FROM server_outbox
         WHERE account_id = ?1 AND action_id = ?2 AND state = 'pending'",
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
             WHERE account_id = ?1 AND action_id = {action_id}
               AND state = 'done' AND generation = {generation}
             ORDER BY id"
        ),
        account_id,
    )?;
    let now = now_unix();
    for entry in applied {
        queue_reverse(conn, account_id, &entry, now)?;
    }
    Ok(())
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
}

fn insert(conn: &Connection, account_id: &AccountId, row: &NewRow<'_>, now: i64) -> Result<()> {
    conn.execute(
        "INSERT INTO server_outbox
            (account_id, kind, state, action_id, message_id, source_folder, source_mailbox,
             uidvalidity, uid, target_folder, target_mailbox, created_unix, generation)
         VALUES (?1, ?2, 'pending', ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11,
                 (SELECT COALESCE(MAX(generation), 0) FROM write_back_accounts
                  WHERE account_id = ?1))",
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
               AND source_mailbox = ?2 AND uidvalidity = ?3 AND uid = ?4
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
             WHERE account_id = ?1 AND message_id = ?2 AND generation = ?3
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
            (placed_uidvalidity, placed_uid, attempts, retry_unix, error, generation, sent),
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
        "DELETE FROM server_outbox
         WHERE account_id = ?1 AND state IN ('done', 'dropped') AND settled_unix < ?2",
        params![account_id.as_str(), now_unix() - KEEP_SETTLED_SECS],
    )?;
    let generation = generation(&tx, account_id)?;
    let entries = select_entries(
        &tx,
        &format!(
            "SELECT {ENTRY_COLS} FROM server_outbox
             WHERE account_id = ?1 AND state = 'pending'
               AND generation = {generation}
             ORDER BY id"
        ),
        account_id,
    )?;
    tx.commit()?;
    Ok(entries)
}

/// Whether any row is due now, without opening a connection (or running
/// `password_command`) when none is.
pub(crate) fn any_due(store: &Store, account_id: &AccountId) -> Result<bool> {
    let tx = crate::store::immediate_transaction(store.connection())?;
    ensure_schema(&tx)?;
    let due: Option<i64> = tx
        .query_row(
            "SELECT 1 FROM server_outbox
             WHERE account_id = ?1 AND state = 'pending' AND retry_unix <= ?2
               AND generation = (SELECT COALESCE(MAX(generation), 0)
                                 FROM write_back_accounts WHERE account_id = ?1)
             LIMIT 1",
            params![account_id.as_str(), now_unix()],
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

/// Record that a MOVE for these rows is about to go out. `target` is the
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
pub(crate) fn take_abandoned(store: &Store, account_id: &AccountId) -> Result<Vec<DroppedMove>> {
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
        "UPDATE server_outbox SET state = 'dropped' WHERE account_id = ?1 AND state = 'abandoned'",
        params![account_id.as_str()],
    )?;
    tx.commit()?;
    Ok(abandoned
        .into_iter()
        .map(|entry| DroppedMove {
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
    Done {
        placed: (u32, u32),
    },
    /// Try again at `retry_unix`. `counts` is false for a dropped connection,
    /// which says nothing about the row. `refused`: the server answered the
    /// MOVE with NO or BAD, so it did not move it, and the next try is a
    /// first send again.
    Retry {
        error: &'a str,
        retry_unix: i64,
        counts: bool,
        refused: bool,
    },
    Dropped {
        error: &'a str,
    },
}

/// Record `outcome` for `entry`, whose copy was resolved to
/// `(source_mailbox, uidvalidity, uid)` and target to `target_mailbox`.
///
/// A sent move also moves the copy in `message_copies`, so a later action
/// finds the message where the server now has it. If undo removed the row
/// while it was being sent, the server acted anyway, so the reverse is
/// queued here. A row from an earlier write-back generation (write-back
/// was turned off while it was on the wire) is left as it is, and `false`
/// comes back: the pass must send nothing more.
pub(crate) fn settle(
    store: &Store,
    account_id: &AccountId,
    entry: &OutboxEntry,
    resolved: Option<(&str, u32, u32)>,
    target_mailbox: Option<&str>,
    outcome: Outcome<'_>,
) -> Result<bool> {
    let tx = crate::store::immediate_transaction(store.connection())?;
    ensure_schema(&tx)?;
    let now = now_unix();
    let live: Option<i64> = tx
        .query_row(
            "SELECT generation FROM server_outbox WHERE id = ?1",
            params![entry.id],
            |row| row.get(0),
        )
        .optional()?;
    let current = enabled(&tx, account_id)?
        .then(|| generation(&tx, account_id))
        .transpose()?;
    if live.or(Some(entry.generation)) != current {
        tx.commit()?;
        return Ok(false);
    }
    let placed = match outcome {
        Outcome::Done { placed } => placed,
        Outcome::Retry {
            error,
            retry_unix,
            counts,
            refused,
        } => {
            tx.execute(
                "UPDATE server_outbox
                 SET attempts = attempts + ?2, retry_unix = ?3, error = ?4,
                     sent = CASE WHEN ?5 THEN 0 ELSE sent END
                 WHERE id = ?1",
                params![entry.id, i64::from(counts), retry_unix, error, refused],
            )?;
            tx.commit()?;
            return Ok(true);
        }
        Outcome::Dropped { error } => {
            tx.execute(
                "UPDATE server_outbox SET state = 'dropped', error = ?2, settled_unix = ?3
                 WHERE id = ?1",
                params![entry.id, error, now],
            )?;
            tx.commit()?;
            return Ok(true);
        }
    };
    let (source_mailbox, uidvalidity, uid) =
        resolved.ok_or_else(|| Error::Action("a sent move needs its copy".into()))?;
    // Undo ran while this was on the wire: it found the row still pending
    // and removed it, but the server has acted on it. Put the row back (no
    // longer tied to the action), then reverse it.
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
    Ok(true)
}

/// The server moved `sent`'s copy: record it where it went. The source copy
/// goes only together with a known new one, so the message never loses
/// every copy it has on record.
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
    conn.execute(
        "DELETE FROM message_copies
         WHERE account_id = ?1 AND mailbox = ?2 AND uidvalidity = ?3 AND uid = ?4",
        params![
            account_id.as_str(),
            crate::imap::escape_mailbox(source),
            uidvalidity,
            uid
        ],
    )?;
    let row_exists: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM messages WHERE account_id = ?1 AND message_id = ?2",
            params![account_id.as_str(), sent.message_id],
            |row| row.get(0),
        )
        .optional()?;
    if row_exists.is_none() {
        return Ok(());
    }
    conn.execute(
        "INSERT INTO message_copies (account_id, mailbox, uidvalidity, uid, message_id, folder)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(account_id, mailbox, uidvalidity, uid) DO NOTHING",
        params![
            account_id.as_str(),
            crate::imap::escape_mailbox(target),
            placed_uidvalidity,
            placed_uid,
            sent.message_id,
            crate::imap::folder_name(target),
        ],
    )?;
    Ok(())
}

pub(crate) fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}
