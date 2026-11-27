//! Read, unread, and star on threads, as local actions undo can reverse.
//!
//! One call covers every thread the reader selected and is one undo entry.
//! Each changed message keeps its previous read and star state for undo,
//! and the new value is recorded as a local intent, so a sync pass that
//! still reports the old server value does not write over it.

use rusqlite::{Connection, OptionalExtension, params};

use super::{ActionKind, LocalAction, now_unix};
use crate::account::AccountId;
use crate::store::{Store, intents, split_field, views};
use crate::threading::ThreadId;
use crate::{Error, Result};

pub(super) const FLAG_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS local_action_flags (
    action_id INTEGER NOT NULL,
    message_id TEXT NOT NULL,
    unread INTEGER NOT NULL,
    starred INTEGER NOT NULL,
    FOREIGN KEY(action_id) REFERENCES local_actions(id)
);

CREATE INDEX IF NOT EXISTS idx_local_action_flags ON local_action_flags(action_id);
"#;

/// What [`Store::flag_threads`] does to each thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlagChange {
    /// Every message in the thread is read.
    Read,
    /// The thread's latest message in the folder on screen is unread, so
    /// the row shows unread there and reopening expands only that message.
    Unread,
    /// The latest message in the folder on screen is starred, unless a
    /// message in that folder already is.
    Star,
    /// No message in the thread is starred.
    Unstar,
}

impl FlagChange {
    fn kind(self) -> ActionKind {
        match self {
            Self::Read => ActionKind::Read,
            Self::Unread => ActionKind::Unread,
            Self::Star => ActionKind::Star,
            Self::Unstar => ActionKind::Unstar,
        }
    }

    fn summary(self, threads: usize) -> String {
        let what = match self {
            Self::Read => "marked read",
            Self::Unread => "marked unread",
            Self::Star => "starred",
            Self::Unstar => "unstarred",
        };
        if threads == 1 {
            format!("{what} 1 thread")
        } else {
            format!("{what} {threads} threads")
        }
    }
}

struct Row {
    message_id: String,
    folders: Vec<String>,
    archived: bool,
    unread: bool,
    starred: bool,
    has_view_copy: bool,
}

impl Store {
    /// Whether all selected folder rows have the given flag, for a toggle
    /// that spans several stores. Missing rows are ignored.
    pub fn folder_selection_flags(
        &self,
        account: &AccountId,
        threads: &[ThreadId],
        folder: &str,
        star: bool,
    ) -> Result<(usize, bool)> {
        self.read_snapshot(|| {
            let mut found = 0;
            let mut all_marked = true;
            for thread in threads {
                if self.folder_key_of(account, folder, thread)?.is_none() {
                    continue;
                }
                found += 1;
                let rows = load_flags(&self.conn, account, thread)?;
                let marked = if star {
                    rows.iter().any(|row| row.starred)
                } else {
                    rows.iter().any(|row| row.unread && in_folder(row, folder))
                };
                all_marked &= marked;
            }
            Ok((found, all_marked))
        })
    }
    /// Apply `change` to each of `threads`, as one undo entry.
    ///
    /// `folder` is the folder on screen; unread and star go on the latest
    /// message there (the latest in the thread when none is). `Ok(None)`
    /// when nothing needed to change, such as opening a thread that is
    /// already read, so no undo entry is written.
    pub fn flag_threads(
        &self,
        account_id: &AccountId,
        threads: &[ThreadId],
        folder: &str,
        change: FlagChange,
    ) -> Result<Option<LocalAction>> {
        self.apply_flags(
            account_id,
            threads.iter().map(|thread| (thread, folder)),
            change,
            false,
        )
    }

    /// Apply one undoable edit to ranked results, using each hit's source folder.
    pub fn flag_scoped_threads(
        &self,
        account_id: &AccountId,
        threads: &[(ThreadId, String)],
        change: FlagChange,
    ) -> Result<Option<LocalAction>> {
        self.apply_flags(
            account_id,
            threads
                .iter()
                .map(|(thread, folder)| (thread, folder.as_str())),
            change,
            true,
        )
    }

    /// Toggle the flag across a marked selection, including threads outside
    /// the UI's loaded page. The decision and write use the same snapshot.
    pub fn toggle_selected_flags(
        &self,
        account: &AccountId,
        ids: &[String],
        source: crate::store::ListSource<'_>,
        star: bool,
    ) -> Result<(FlagChange, usize)> {
        use crate::store::ListSource;

        self.require_account(account)?;
        let tx = crate::store::immediate_transaction(self.connection())?;
        super::ensure_action_schema(&tx)?;
        let labels = match source {
            ListSource::Label(_) => self.thread_labels(account, ids)?,
            ListSource::Folder(_) => Default::default(),
        };
        let folder = match source {
            ListSource::Folder(folder) => folder,
            ListSource::Label(_) => "",
        };
        let mut selected = Vec::new();
        for id in ids {
            let thread = ThreadId(id.clone());
            let eligible = match source {
                ListSource::Folder(folder) => {
                    self.folder_key_of(account, folder, &thread)?.is_some()
                }
                ListSource::Label(label) => labels
                    .get(id)
                    .is_some_and(|item| item.labels.contains(label)),
            };
            if !eligible {
                continue;
            }
            let rows = load_flags(&tx, account, &thread)?;
            let folder_has_copy = rows.iter().any(|row| in_folder(row, folder));
            let marked = if star {
                rows.iter().any(|row| row.starred)
            } else {
                rows.iter()
                    .any(|row| row.unread && (!folder_has_copy || in_folder(row, folder)))
            };
            selected.push((rows, marked));
        }
        if selected.is_empty() {
            return Err(Error::Action("No conversation".into()));
        }
        let turn_on = !selected.iter().all(|(_, marked)| *marked);
        let change = match (star, turn_on) {
            (true, true) => FlagChange::Star,
            (true, false) => FlagChange::Unstar,
            (false, true) => FlagChange::Unread,
            (false, false) => FlagChange::Read,
        };
        let mut writes = Vec::new();
        let mut touched = 0;
        for (rows, marked) in selected {
            if marked != turn_on {
                let before = writes.len();
                plan(rows, |row| in_folder(row, folder), change, &mut writes);
                touched += usize::from(writes.len() > before);
            }
        }
        if touched > 0 {
            write_flags(&tx, account, change, touched, writes)?;
        }
        tx.commit()?;
        Ok((change, touched))
    }

    fn apply_flags<'a>(
        &self,
        account_id: &AccountId,
        threads: impl IntoIterator<Item = (&'a ThreadId, &'a str)>,
        change: FlagChange,
        scoped: bool,
    ) -> Result<Option<LocalAction>> {
        self.require_account(account_id)?;
        let tx = crate::store::immediate_transaction(self.connection())?;
        let roles = scoped.then(|| self.folder_roles(account_id)).transpose()?;
        let action = apply_flags_in(&tx, account_id, threads, change, roles.as_ref())?;
        tx.commit()?;
        Ok(action)
    }
}

pub(crate) fn apply_flags_in<'a>(
    conn: &Connection,
    account_id: &AccountId,
    threads: impl IntoIterator<Item = (&'a ThreadId, &'a str)>,
    change: FlagChange,
    roles: Option<&crate::FolderRoles>,
) -> Result<Option<LocalAction>> {
    super::ensure_action_schema(conn)?;

    let mut writes: Vec<(Row, bool, bool)> = Vec::new();
    let mut touched = 0;
    let mut found = false;
    for (thread, folder) in threads {
        let mut rows = load_flags(conn, account_id, thread)?;
        let is_here = |row: &Row| match roles {
            Some(roles) => in_source_folder(row, folder, roles.name(crate::FolderRole::Inbox)),
            None => in_folder(row, folder),
        };
        if roles.is_some() {
            // Search uses stored folder identities. Role aliases may
            // name different folders, and a sync can remove this scope.
            rows.retain(&is_here);
            if rows.is_empty() {
                return Err(Error::Action(format!("no messages in {folder}")));
            }
        }
        found |= !rows.is_empty();
        let before = writes.len();
        plan(rows, is_here, change, &mut writes);
        if writes.len() > before {
            touched += 1;
        }
    }
    if !found {
        return Err(Error::Action("no such thread".into()));
    }
    if writes.is_empty() {
        return Ok(None);
    }
    write_flags(conn, account_id, change, touched, writes).map(Some)
}

fn write_flags(
    conn: &Connection,
    account_id: &AccountId,
    change: FlagChange,
    touched: usize,
    writes: Vec<(Row, bool, bool)>,
) -> Result<LocalAction> {
    let kind = change.kind();
    let summary = change.summary(touched);
    conn.execute(
        "INSERT INTO local_actions (account_id, kind, summary, undone, created_unix)
             VALUES (?1, ?2, ?3, 0, ?4)",
        params![account_id.as_str(), kind.as_str(), summary, now_unix()],
    )?;
    let id = conn.last_insert_rowid();
    for (row, unread, starred) in &writes {
        conn.execute(
            "INSERT INTO local_action_flags (action_id, message_id, unread, starred)
                 VALUES (?1, ?2, ?3, ?4)",
            params![
                id,
                row.message_id,
                i64::from(row.unread),
                i64::from(row.starred)
            ],
        )?;
        set_flags(conn, account_id, &row.message_id, *unread, *starred)?;
        intents::record(
            conn,
            account_id,
            &row.message_id,
            (row.unread != *unread).then_some(*unread),
            (row.starred != *starred).then_some(*starred),
        )?;
    }
    Ok(LocalAction {
        id,
        kind,
        summary,
        label_undo_target: None,
    })
}

/// The messages to change in one thread, oldest first, with their new
/// read and star values.
fn plan(
    rows: Vec<Row>,
    is_here: impl Fn(&Row) -> bool,
    change: FlagChange,
    out: &mut Vec<(Row, bool, bool)>,
) {
    let latest_in_folder = rows.iter().rposition(&is_here);
    let latest_here = latest_in_folder.or(rows.len().checked_sub(1));
    let already_starred = rows
        .iter()
        .any(|row| row.starred && (latest_in_folder.is_none() || is_here(row)));
    for (index, row) in rows.into_iter().enumerate() {
        let (unread, starred) = match change {
            FlagChange::Read => (false, row.starred),
            FlagChange::Unread if Some(index) == latest_here => (true, row.starred),
            FlagChange::Star if Some(index) == latest_here && !already_starred => {
                (row.unread, true)
            }
            FlagChange::Unstar => (row.unread, false),
            _ => (row.unread, row.starred),
        };
        if unread != row.unread || starred != row.starred {
            out.push((row, unread, starred));
        }
    }
}

/// Whether `row` shows in `folder`. A message with no folder that is not
/// archived is in the Inbox, as the folder list counts it.
fn in_folder(row: &Row, folder: &str) -> bool {
    if row.folders.is_empty() && !row.archived && !row.has_view_copy {
        return folder.eq_ignore_ascii_case(super::FOLDER_INBOX);
    }
    let aliases = crate::folder_aliases(folder);
    row.folders.iter().any(|name| {
        name.eq_ignore_ascii_case(folder)
            || aliases.iter().any(|alias| name.eq_ignore_ascii_case(alias))
    })
}

fn in_source_folder(row: &Row, folder: &str, inbox: &str) -> bool {
    if row.folders.is_empty() && !row.archived && !row.has_view_copy {
        return folder.eq_ignore_ascii_case(inbox);
    }
    row.folders
        .iter()
        .any(|name| name.eq_ignore_ascii_case(folder))
}

fn load_flags(conn: &Connection, account_id: &AccountId, thread: &ThreadId) -> Result<Vec<Row>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT message_id, folders, archived, unread, starred, ({}), {}
         FROM messages m
         WHERE account_id = ?1 AND thread_id = ?2
         ORDER BY date_unix ASC, rowid ASC",
        views::scopes("m"),
        views::has_view_copy("m"),
    ))?;
    let rows = stmt.query_map(params![account_id.as_str(), thread.0], |row| {
        let mut folders = split_field(&row.get::<_, String>(1)?);
        if let Some(scopes) = row.get::<_, Option<String>>(5)? {
            folders.extend(split_field(&scopes));
        }
        Ok(Row {
            message_id: row.get(0)?,
            folders,
            archived: row.get::<_, i64>(2)? != 0,
            unread: row.get::<_, i64>(3)? != 0,
            starred: row.get::<_, i64>(4)? != 0,
            has_view_copy: row.get(6)?,
        })
    })?;
    rows.collect::<rusqlite::Result<_>>().map_err(Into::into)
}

fn set_flags(
    conn: &Connection,
    account_id: &AccountId,
    message_id: &str,
    unread: bool,
    starred: bool,
) -> Result<()> {
    conn.execute(
        "UPDATE messages SET unread = ?1, starred = ?2
         WHERE account_id = ?3 AND message_id = ?4",
        params![
            i64::from(unread),
            i64::from(starred),
            account_id.as_str(),
            message_id
        ],
    )?;
    Ok(())
}

/// Save a star as part of a folder action's undo entry. A deleted row
/// needs the snapshot too, since restoring its message initializes flags.
pub(super) fn snapshot_star(
    conn: &Connection,
    account: &AccountId,
    action_id: i64,
    message_id: &str,
    clear: bool,
) -> Result<()> {
    let saved = conn.execute(
        "INSERT INTO local_action_flags (action_id, message_id, unread, starred)
         SELECT ?1, message_id, unread, starred FROM messages
         WHERE account_id = ?2 AND message_id = ?3 AND starred",
        params![action_id, account.as_str(), message_id],
    )?;
    if clear && saved != 0 {
        conn.execute(
            "UPDATE messages SET starred = 0 WHERE account_id = ?1 AND message_id = ?2",
            params![account.as_str(), message_id],
        )?;
        intents::record(conn, account, message_id, None, Some(false))?;
    }
    Ok(())
}

/// Restore the flag the action changed, alongside its local intent. The
/// other flag may have changed independently since the action was recorded.
pub(super) fn undo(conn: &Connection, account_id: &AccountId, action: &LocalAction) -> Result<()> {
    let saved: Vec<(String, bool, bool)> = {
        let mut stmt = conn.prepare(
            "SELECT message_id, unread, starred FROM local_action_flags
             WHERE action_id = ?1 ORDER BY rowid ASC",
        )?;
        stmt.query_map(params![action.id], |row| {
            Ok((
                row.get(0)?,
                row.get::<_, i64>(1)? != 0,
                row.get::<_, i64>(2)? != 0,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?
    };
    if saved.is_empty() && action.kind.is_flag() {
        return Err(Error::Action("action has no messages".into()));
    }
    let restore_unread = matches!(action.kind, ActionKind::Read | ActionKind::Unread);
    for (message_id, unread, starred) in &saved {
        let now: Option<(bool, bool)> = conn
            .query_row(
                "SELECT unread, starred FROM messages WHERE account_id = ?1 AND message_id = ?2",
                params![account_id.as_str(), message_id],
                |row| Ok((row.get::<_, i64>(0)? != 0, row.get::<_, i64>(1)? != 0)),
            )
            .optional()?;
        // A message deleted since then has nothing to restore.
        let Some((now_unread, now_starred)) = now else {
            continue;
        };
        let (unread, starred) = if restore_unread {
            (*unread, now_starred)
        } else {
            (now_unread, *starred)
        };
        set_flags(conn, account_id, message_id, unread, starred)?;
        intents::record(
            conn,
            account_id,
            message_id,
            (now_unread != unread).then_some(unread),
            (now_starred != starred).then_some(starred),
        )?;
    }
    conn.execute(
        "DELETE FROM local_action_flags WHERE action_id = ?1",
        params![action.id],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{Account, BackendKind};
    use crate::backend::FlagUpdate;
    use crate::message::{Address, Message, MessageHeaders, MessageId, RemoteId};
    use crate::{MailboxDelta, PendingFlags};

    fn store() -> (Store, AccountId) {
        let store = Store::open_in_memory().unwrap();
        let account = AccountId("a1".into());
        store
            .upsert_account(&Account {
                id: account.clone(),
                display_name: "Me".into(),
                email: "me@x".into(),
                backend_kind: BackendKind::Bridge,
            })
            .unwrap();
        (store, account)
    }

    fn message(id: &str, parent: Option<&str>, folder: &str, date: i64) -> Message {
        Message {
            account_id: AccountId("a1".into()),
            headers: MessageHeaders {
                message_id: MessageId::new(id),
                proton_conversation_id: None,
                in_reply_to: parent.map(MessageId::new),
                references: parent.map(MessageId::new).into_iter().collect(),
                from: Address {
                    name: None,
                    email: "ada@x".into(),
                },
                subject: "Plans".into(),
                date_unix: date,
            },
            remote_id: RemoteId(format!("r:{id}")),
            thread_id: ThreadId::from_message_id(&MessageId::new(id)),
            snippet: "hello".into(),
            body_text: Some("hello".into()),
            unread: true,
            folders: vec![folder.into()],
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
            reply_to: Some(Vec::new()),
            attachment_names: Vec::new(),
            parts: None,
        }
    }

    /// A root and a reply in the inbox, then my answer in Sent (latest).
    fn conversation(store: &Store, root: &str) -> ThreadId {
        let reply = format!("{root}-reply");
        let mine = format!("{root}-mine");
        let thread = store
            .upsert_message(&message(root, None, "INBOX", 100))
            .unwrap();
        store
            .upsert_message(&message(&reply, Some(root), "INBOX", 200))
            .unwrap();
        let mut sent = message(&mine, Some(&reply), "Sent", 300);
        sent.unread = false;
        store.upsert_message(&sent).unwrap();
        thread
    }

    fn flags(store: &Store, account: &AccountId, thread: &ThreadId) -> Vec<(bool, bool)> {
        store
            .thread_messages(account, thread)
            .unwrap()
            .iter()
            .map(|message| (message.unread, message.starred))
            .collect()
    }

    fn inbox_unread(store: &Store, account: &AccountId, thread: &ThreadId) -> bool {
        store
            .folder_threads(account, "INBOX")
            .unwrap()
            .into_iter()
            .find(|row| &row.thread_id == thread)
            .unwrap()
            .unread
    }

    fn flag_delta(id: &str, unread: bool) -> MailboxDelta {
        MailboxDelta {
            flags: vec![FlagUpdate {
                remote_id: RemoteId(format!("r:{id}")),
                unread: Some(unread),
                flagged: None,
                keywords: None,
                delivery_keywords: None,
                modseq: None,
            }],
            ..MailboxDelta::default()
        }
    }

    #[test]
    fn read_marks_the_whole_thread_and_undo_brings_unread_back() {
        let (store, account) = store();
        let thread = conversation(&store, "a@x");
        let action = store
            .flag_threads(
                &account,
                std::slice::from_ref(&thread),
                "INBOX",
                FlagChange::Read,
            )
            .unwrap()
            .unwrap();
        assert_eq!(action.kind, ActionKind::Read);
        assert_eq!(action.summary, "marked read 1 thread");
        assert!(flags(&store, &account, &thread).iter().all(|f| !f.0));
        assert!(!inbox_unread(&store, &account, &thread));
        // Opening it again changes nothing and logs nothing.
        assert!(
            store
                .flag_threads(
                    &account,
                    std::slice::from_ref(&thread),
                    "INBOX",
                    FlagChange::Read
                )
                .unwrap()
                .is_none()
        );
        assert_eq!(store.undo(&account).unwrap().kind, ActionKind::Read);
        assert_eq!(
            flags(&store, &account, &thread),
            vec![(true, false), (true, false), (false, false)]
        );
        assert!(inbox_unread(&store, &account, &thread));
        assert!(store.undo(&account).is_err(), "one entry, undone once");
    }

    #[test]
    fn unread_marks_the_latest_message_in_the_folder_on_screen() {
        let (store, account) = store();
        let thread = conversation(&store, "a@x");
        store
            .flag_threads(
                &account,
                std::slice::from_ref(&thread),
                "INBOX",
                FlagChange::Read,
            )
            .unwrap();
        let action = store
            .flag_threads(
                &account,
                std::slice::from_ref(&thread),
                "inbox",
                FlagChange::Unread,
            )
            .unwrap()
            .unwrap();
        assert_eq!(action.kind, ActionKind::Unread);
        // The reply, not my answer in Sent, so the inbox row is unread.
        assert_eq!(
            flags(&store, &account, &thread),
            vec![(false, false), (true, false), (false, false)]
        );
        assert!(inbox_unread(&store, &account, &thread));
        store.undo(&account).unwrap();
        assert!(!inbox_unread(&store, &account, &thread));
    }

    #[test]
    fn star_and_unstar_are_undoable() {
        let (store, account) = store();
        let thread = conversation(&store, "a@x");
        store
            .flag_threads(
                &account,
                std::slice::from_ref(&thread),
                "INBOX",
                FlagChange::Star,
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            flags(&store, &account, &thread),
            vec![(true, false), (true, true), (false, false)]
        );
        assert!(
            store
                .folder_threads(&account, "INBOX")
                .unwrap()
                .iter()
                .any(|row| row.thread_id == thread && row.starred)
        );
        assert!(
            store
                .flag_threads(
                    &account,
                    std::slice::from_ref(&thread),
                    "INBOX",
                    FlagChange::Star
                )
                .unwrap()
                .is_none(),
            "a starred thread stays as it is"
        );
        store
            .flag_threads(
                &account,
                std::slice::from_ref(&thread),
                "INBOX",
                FlagChange::Unstar,
            )
            .unwrap()
            .unwrap();
        assert!(flags(&store, &account, &thread).iter().all(|f| !f.1));
        assert_eq!(store.undo(&account).unwrap().kind, ActionKind::Unstar);
        assert!(flags(&store, &account, &thread)[1].1);
        assert_eq!(store.undo(&account).unwrap().kind, ActionKind::Star);
        assert!(flags(&store, &account, &thread).iter().all(|f| !f.1));
    }

    #[test]
    fn an_existing_star_in_sent_does_not_block_starring_inbox() {
        let (store, account) = store();
        let thread = conversation(&store, "a@x");
        for folder in ["Sent", "INBOX"] {
            assert!(
                store
                    .flag_threads(
                        &account,
                        std::slice::from_ref(&thread),
                        folder,
                        FlagChange::Star,
                    )
                    .unwrap()
                    .is_some()
            );
        }
        assert_eq!(
            flags(&store, &account, &thread),
            vec![(true, false), (true, true), (false, true)]
        );
    }

    #[test]
    fn undo_star_preserves_a_read_change_from_sync() {
        let (store, account) = store();
        let thread = conversation(&store, "a@x");
        store
            .flag_threads(
                &account,
                std::slice::from_ref(&thread),
                "INBOX",
                FlagChange::Star,
            )
            .unwrap();
        store
            .apply_delta(&account, &flag_delta("a@x-reply", false))
            .unwrap();
        assert_eq!(flags(&store, &account, &thread)[1], (false, true));

        assert_eq!(store.undo(&account).unwrap().kind, ActionKind::Star);
        assert_eq!(flags(&store, &account, &thread)[1], (false, false));
    }

    #[test]
    fn five_threads_are_one_undo_entry() {
        let (store, account) = store();
        let threads: Vec<ThreadId> = (0..5)
            .map(|n| conversation(&store, &format!("t{n}@x")))
            .collect();
        let action = store
            .flag_threads(&account, &threads, "INBOX", FlagChange::Read)
            .unwrap()
            .unwrap();
        assert_eq!(action.summary, "marked read 5 threads");
        for thread in &threads {
            assert!(!inbox_unread(&store, &account, thread));
        }
        store.undo(&account).unwrap();
        for thread in &threads {
            assert!(inbox_unread(&store, &account, thread));
        }
    }

    #[test]
    fn a_purge_retires_only_the_affected_messages_in_a_flag_action() {
        let (store, account) = store();
        let mut purged = message("remove@x", None, "Trash", 100);
        purged.folders.push("Sent".into());
        let purged = store.upsert_message(&purged).unwrap();
        let mut unaffected = message("keep@x", None, "INBOX", 200);
        unaffected.headers.subject = "Unrelated mail".into();
        let unaffected = store.upsert_message(&unaffected).unwrap();
        store
            .flag_threads(
                &account,
                &[purged.clone(), unaffected.clone()],
                "INBOX",
                FlagChange::Star,
            )
            .unwrap()
            .unwrap();

        store
            .purge_threads(&account, std::slice::from_ref(&purged), "Trash")
            .unwrap();
        assert_eq!(store.undo(&account).unwrap().kind, ActionKind::Star);
        assert!(
            flags(&store, &account, &purged)[0].1,
            "the Sent copy stays starred"
        );
        assert!(!flags(&store, &account, &unaffected)[0].1);
    }

    #[test]
    fn an_unknown_thread_is_an_error() {
        let (store, account) = store();
        let err = store
            .flag_threads(
                &account,
                &[ThreadId("nope".into())],
                "INBOX",
                FlagChange::Read,
            )
            .unwrap_err();
        assert_eq!(err.to_string(), "no such thread");
    }

    #[test]
    fn sync_still_reporting_unread_keeps_a_local_read() {
        let (store, account) = store();
        let thread = conversation(&store, "a@x");
        store
            .flag_threads(
                &account,
                std::slice::from_ref(&thread),
                "INBOX",
                FlagChange::Read,
            )
            .unwrap();
        // A flags-only pass and a full resync both still say unread.
        store
            .apply_delta(&account, &flag_delta("a@x", true))
            .unwrap();
        store
            .apply_delta(
                &account,
                &MailboxDelta {
                    messages: vec![message("a@x-reply", Some("a@x"), "INBOX", 200)],
                    ..MailboxDelta::default()
                },
            )
            .unwrap();
        assert!(flags(&store, &account, &thread).iter().all(|f| !f.0));
        assert!(!inbox_unread(&store, &account, &thread));
        assert_eq!(store.pending_flag_changes(&account).unwrap().len(), 2);

        // The server catches up on one message: that intent is done, and a
        // later unread from the server (another client) wins again.
        store
            .apply_delta(&account, &flag_delta("a@x", false))
            .unwrap();
        assert_eq!(
            store.pending_flag_changes(&account).unwrap(),
            vec![PendingFlags {
                message_id: "a@x-reply".into(),
                unread: Some(false),
                starred: None,
            }]
        );
        store
            .apply_delta(&account, &flag_delta("a@x", true))
            .unwrap();
        assert!(flags(&store, &account, &thread)[0].0);
    }

    #[test]
    fn sync_does_not_take_back_a_local_unstar() {
        let (store, account) = store();
        let mut starred = message("s@x", None, "INBOX", 100);
        starred.starred = true;
        let thread = store.upsert_message(&starred).unwrap();
        store
            .flag_threads(
                &account,
                std::slice::from_ref(&thread),
                "INBOX",
                FlagChange::Unstar,
            )
            .unwrap()
            .unwrap();
        store
            .apply_delta(
                &account,
                &MailboxDelta {
                    messages: vec![starred.clone()],
                    ..MailboxDelta::default()
                },
            )
            .unwrap();
        assert!(!flags(&store, &account, &thread)[0].1);
    }

    #[test]
    fn undo_is_the_intent_sync_keeps() {
        let (store, account) = store();
        let thread = store
            .upsert_message(&message("u@x", None, "INBOX", 100))
            .unwrap();
        store
            .flag_threads(
                &account,
                std::slice::from_ref(&thread),
                "INBOX",
                FlagChange::Read,
            )
            .unwrap();
        store.undo(&account).unwrap();
        assert_eq!(
            store.pending_flag_changes(&account).unwrap()[0].unread,
            Some(true)
        );
        // The server never saw the read: it agrees, so nothing is pending.
        store
            .apply_delta(&account, &flag_delta("u@x", true))
            .unwrap();
        assert!(store.pending_flag_changes(&account).unwrap().is_empty());
        assert!(flags(&store, &account, &thread)[0].0);
    }

    #[test]
    fn an_agent_read_also_survives_sync() {
        let (store, account) = store();
        let thread = store
            .upsert_message(&message("g@x", None, "INBOX", 100))
            .unwrap();
        store
            .mark_thread_read(&account, &thread, "agent")
            .unwrap()
            .unwrap();
        store
            .apply_delta(&account, &flag_delta("g@x", true))
            .unwrap();
        assert!(!flags(&store, &account, &thread)[0].0);
        store.agent_undo(&account).unwrap();
        assert_eq!(
            store.pending_flag_changes(&account).unwrap()[0].unread,
            Some(true)
        );
    }

    #[test]
    fn agent_unread_and_star_are_intents_and_so_are_their_undos() {
        let (store, account) = store();
        let mut read = message("h@x", None, "INBOX", 100);
        read.unread = false;
        let thread = store.upsert_message(&read).unwrap();
        store
            .mark_thread_unread(&account, &thread, "agent")
            .unwrap()
            .unwrap();
        store
            .star_thread(&account, &thread, true, "agent")
            .unwrap()
            .unwrap();
        assert_eq!(
            store.pending_flag_changes(&account).unwrap(),
            vec![PendingFlags {
                message_id: "h@x".into(),
                unread: Some(true),
                starred: Some(true),
            }]
        );
        // A resync that still says read and unstarred takes back neither.
        store
            .apply_delta(
                &account,
                &MailboxDelta {
                    messages: vec![read.clone()],
                    ..MailboxDelta::default()
                },
            )
            .unwrap();
        assert_eq!(flags(&store, &account, &thread)[0], (true, true));
        // Undo of the star is an unstar intent, not a dropped one.
        store.agent_undo(&account).unwrap();
        let pending = store.pending_flag_changes(&account).unwrap();
        assert_eq!(pending[0].starred, Some(false));
        assert_eq!(pending[0].unread, Some(true));
        store.agent_undo(&account).unwrap();
        assert_eq!(
            store.pending_flag_changes(&account).unwrap()[0].unread,
            Some(false)
        );
    }

    #[test]
    fn deleting_a_message_drops_its_intent() {
        let (store, account) = store();
        let thread = store
            .upsert_message(&message("d@x", None, "INBOX", 100))
            .unwrap();
        store
            .flag_threads(&account, &[thread], "INBOX", FlagChange::Read)
            .unwrap();
        store
            .delete_message(&account, &MessageId::new("d@x"))
            .unwrap();
        assert!(store.pending_flag_changes(&account).unwrap().is_empty());
    }

    /// The same message as an INBOX copy and a Sent copy, both unread, on
    /// one row; the user has read it in Fell.
    fn two_copies(store: &Store, account: &AccountId) -> ThreadId {
        let mut inbox = message("c@x", None, "INBOX", 100);
        inbox.remote_id = RemoteId("imap:1:5:INBOX".into());
        let mut sent = message("c@x", None, "Sent", 100);
        sent.remote_id = RemoteId("imap:1:9:Sent".into());
        store
            .apply_delta(
                account,
                &MailboxDelta {
                    messages: vec![inbox, sent],
                    ..MailboxDelta::default()
                },
            )
            .unwrap();
        let thread = ThreadId::from_message_id(&MessageId::new("c@x"));
        store
            .flag_threads(
                account,
                std::slice::from_ref(&thread),
                "INBOX",
                FlagChange::Read,
            )
            .unwrap()
            .unwrap();
        thread
    }

    fn copy_delta(remote: &str, unread: bool) -> MailboxDelta {
        MailboxDelta {
            flags: vec![FlagUpdate {
                remote_id: RemoteId(remote.into()),
                unread: Some(unread),
                flagged: None,
                keywords: None,
                delivery_keywords: None,
                modseq: None,
            }],
            ..MailboxDelta::default()
        }
    }

    #[test]
    fn a_local_read_waits_for_every_server_copy() {
        for sent_first in [true, false] {
            let (store, account) = store();
            let thread = two_copies(&store, &account);
            let sent_agrees = copy_delta("imap:1:9:Sent", false);
            let inbox_still_unread = copy_delta("imap:1:5:INBOX", true);
            let (first, second) = if sent_first {
                (&sent_agrees, &inbox_still_unread)
            } else {
                (&inbox_still_unread, &sent_agrees)
            };
            store.apply_delta(&account, first).unwrap();
            store.apply_delta(&account, second).unwrap();
            assert!(
                !flags(&store, &account, &thread)[0].0,
                "one copy agreeing must not let the other take the read back (sent first: {sent_first})"
            );
            assert_eq!(store.pending_flag_changes(&account).unwrap().len(), 1);

            // The INBOX copy catches up too: now the server agrees.
            store
                .apply_delta(&account, &copy_delta("imap:1:5:INBOX", false))
                .unwrap();
            assert!(store.pending_flag_changes(&account).unwrap().is_empty());
            // Later server changes win again.
            store.apply_delta(&account, &inbox_still_unread).unwrap();
            assert!(flags(&store, &account, &thread)[0].0);
        }
    }

    #[test]
    fn a_flags_only_update_keeps_a_local_read_whichever_way_it_finds_the_row() {
        // FLAGS-only updates find their row by its remote id, by an alias
        // (another Message-ID folded onto it), or by a copy recorded for it
        // (#175). Each of those must keep the read made in Fell.
        let (store, account) = store();
        let thread = two_copies(&store, &account);
        store
            .conn
            .execute(
                "INSERT INTO message_aliases (
                    account_id, message_id, canonical_message_id, remote_id, folder
                 ) VALUES (?1, 'list-copy@x', 'c@x', 'imap:1:7:Lists', 'Lists')",
                rusqlite::params![account.as_str()],
            )
            .unwrap();
        for (remote, path) in [
            ("imap:1:5:INBOX", "remote id"),
            ("imap:1:7:Lists", "alias"),
            ("imap:1:9:Sent", "recorded copy"),
        ] {
            store
                .apply_delta(&account, &copy_delta(remote, true))
                .unwrap();
            assert!(
                !flags(&store, &account, &thread)[0].0,
                "a FLAGS-only unread found by its {path} took back the read"
            );
        }
        assert_eq!(store.pending_flag_changes(&account).unwrap().len(), 1);
    }

    #[test]
    fn a_copy_that_disagrees_again_is_waited_for_again() {
        let (store, account) = store();
        let thread = two_copies(&store, &account);
        store
            .apply_delta(&account, &copy_delta("imap:1:9:Sent", false))
            .unwrap();
        store
            .apply_delta(&account, &copy_delta("imap:1:9:Sent", true))
            .unwrap();
        store
            .apply_delta(&account, &copy_delta("imap:1:5:INBOX", false))
            .unwrap();
        assert_eq!(store.pending_flag_changes(&account).unwrap().len(), 1);
        store
            .apply_delta(&account, &copy_delta("imap:1:5:INBOX", true))
            .unwrap();
        assert!(!flags(&store, &account, &thread)[0].0);
    }

    #[test]
    fn a_local_star_stays_pending_until_write_back() {
        // Sync reports every copy unstarred until it reads \Flagged (#102),
        // which says nothing about whether the server saw the change.
        let (store, account) = store();
        let plain = message("p@x", None, "INBOX", 100);
        let thread = store.upsert_message(&plain).unwrap();
        store
            .flag_threads(
                &account,
                std::slice::from_ref(&thread),
                "INBOX",
                FlagChange::Star,
            )
            .unwrap()
            .unwrap();
        let resync = MailboxDelta {
            messages: vec![plain.clone()],
            ..MailboxDelta::default()
        };
        store.apply_delta(&account, &resync).unwrap();
        assert!(flags(&store, &account, &thread)[0].1);
        assert_eq!(
            store.pending_flag_changes(&account).unwrap(),
            vec![PendingFlags {
                message_id: "p@x".into(),
                unread: None,
                starred: Some(true),
            }]
        );

        // An unstar is not dropped by a sync that happens to match it.
        store
            .flag_threads(
                &account,
                std::slice::from_ref(&thread),
                "INBOX",
                FlagChange::Unstar,
            )
            .unwrap()
            .unwrap();
        store.apply_delta(&account, &resync).unwrap();
        assert_eq!(
            store.pending_flag_changes(&account).unwrap()[0].starred,
            Some(false)
        );
    }

    #[test]
    fn a_message_with_no_folder_counts_as_inbox() {
        // The folder list shows a row with no folder (and not archived) in
        // the Inbox, so U and s there land on it, not my later Sent reply.
        let (store, account) = store();
        let mut root = message("n@x", None, "INBOX", 100);
        root.folders.clear();
        root.unread = false;
        let thread = store.upsert_message(&root).unwrap();
        let mut mine = message("n@x-mine", Some("n@x"), "Sent", 200);
        mine.unread = false;
        store.upsert_message(&mine).unwrap();
        store
            .flag_threads(
                &account,
                std::slice::from_ref(&thread),
                "INBOX",
                FlagChange::Unread,
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            flags(&store, &account, &thread),
            vec![(true, false), (false, false)]
        );
        assert!(inbox_unread(&store, &account, &thread));
        store
            .flag_threads(
                &account,
                std::slice::from_ref(&thread),
                "INBOX",
                FlagChange::Star,
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            flags(&store, &account, &thread),
            vec![(true, true), (false, false)]
        );
    }
}
