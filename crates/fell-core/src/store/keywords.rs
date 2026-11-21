//! IMAP keyword evidence lives on each server copy. The row remembers only
//! the labels sync introduced, so a server removal cannot take a local label
//! with the same name. Labels on older rows have no provenance and stay local.

use rusqlite::{Connection, OptionalExtension, params};

use crate::account::AccountId;
use crate::imap::{RemoteCopy, is_reserved_keyword};
use crate::message::MessageId;
use crate::search::{IndexedText, sync_terms, terms_for_indexed};
use crate::{Error, Result};

use super::{Arrival, find_stored, join_tokens, split_field, union_tokens};

pub(super) fn observe(
    conn: &Connection,
    account_id: &AccountId,
    copy: &RemoteCopy,
    modseq: Option<u64>,
    keywords: &[String],
    bridge: bool,
) -> Result<()> {
    let mut unique = Vec::new();
    for keyword in keywords {
        if !contains(&unique, keyword) {
            unique.push(keyword.clone());
        }
    }
    let encoded = join_tokens(&unique);
    let previous: Option<(String, String)> = conn
        .prepare_cached(
            "SELECT message_id, keywords FROM message_copies
             WHERE account_id = ?1 AND mailbox = ?2 AND uidvalidity = ?3 AND uid = ?4
               AND keywords != ?5 AND (?6 IS NULL OR modseq IS NULL OR modseq <= ?6)",
        )?
        .query_row(
            params![
                account_id.as_str(),
                copy.mailbox,
                copy.uidvalidity,
                copy.uid,
                encoded,
                modseq.map(u64::to_be_bytes)
            ],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((message_id, previous)) = previous else {
        return Ok(());
    };
    let before = split_field(&previous);
    // A Bridge observation carries a label mailbox's name. Generic IMAP
    // observations carry raw FLAGS keywords, which we keep on the copy.
    let is_label = |keyword: &str| bridge || !is_reserved_keyword(keyword);
    let removed = before
        .iter()
        .any(|keyword| is_label(keyword) && !contains(&unique, keyword));
    let added: Vec<_> = unique
        .into_iter()
        .filter(|keyword| is_label(keyword) && !contains(&before, keyword))
        .collect();
    if !added.is_empty() {
        expire_readd_suppression(conn, account_id, &message_id, &added)?;
    }
    conn.prepare_cached(
        "UPDATE message_copies SET keywords = ?5
         WHERE account_id = ?1 AND mailbox = ?2 AND uidvalidity = ?3 AND uid = ?4",
    )?
    .execute(params![
        account_id.as_str(),
        copy.mailbox,
        copy.uidvalidity,
        copy.uid,
        encoded
    ])?;
    reconcile(conn, account_id, &message_id, &added, removed)?;
    Ok(())
}

/// Apply additions from one copy and retire labels when their last support
/// goes away. An unchanged snapshot leaves a label removed locally alone.
pub(super) fn reconcile(
    conn: &Connection,
    account_id: &AccountId,
    message_id: &str,
    added: &[String],
    check_removed: bool,
) -> Result<bool> {
    let mut managed = managed_labels(conn, account_id, message_id)?;
    let before = managed.clone();
    if check_removed && !managed.is_empty() {
        let supported = supported_keywords(conn, account_id, message_id)?;
        managed.retain(|keyword| contains(&supported, keyword));
    }
    if added.is_empty() && managed == before {
        return Ok(false);
    }
    let Some((stored, _)) = find_stored(conn, account_id, &MessageId(message_id.into()))? else {
        return Ok(false);
    };
    let mut visible = split_field(&stored.labels);
    // The marker also names the exact chip that sync owns. A local label
    // with different spelling can coexist, even though copy support folds case.
    visible.retain(|label| !before.contains(label) || managed.contains(label));
    for keyword in added {
        if !contains(&visible, keyword) && !contains(&managed, keyword) {
            visible.push(keyword.clone());
            managed.push(keyword.clone());
        }
    }
    if managed != before {
        save_managed(conn, account_id, &stored.canonical_id, &managed)?;
    }
    write_labels(conn, account_id, &stored, &join_tokens(&visible))
}

fn write_labels(
    conn: &Connection,
    account_id: &AccountId,
    stored: &Arrival,
    labels: &str,
) -> Result<bool> {
    if stored.labels == labels {
        return Ok(false);
    }
    let indexed = |labels| {
        terms_for_indexed(IndexedText {
            subject: &stored.subject,
            from_name: stored.from_name.as_deref(),
            from_email: &stored.from_email,
            body: &stored.body,
            to_line: &stored.to_line,
            cc_line: &stored.cc_line,
            bcc_line: &stored.bcc_line,
            folders: &stored.folders,
            labels,
        })
    };
    conn.prepare_cached(
        "UPDATE messages SET labels = ?3 WHERE account_id = ?1 AND message_id = ?2",
    )?
    .execute(params![account_id.as_str(), stored.canonical_id, labels])?;
    sync_terms(
        conn,
        account_id.as_str(),
        &indexed(&stored.labels),
        &indexed(labels),
    )?;
    Ok(true)
}

/// Earlier versions copied all IMAP keywords into labels. Clean the label
/// projection and its ownership markers without changing server observations.
pub(super) fn migrate_reserved_labels(conn: &Connection) -> Result<()> {
    const REPAIR: &str = "reserved_imap_labels_v1";
    let done = |conn: &Connection| {
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM meta WHERE key = ?1)",
            [REPAIR],
            |row| row.get::<_, bool>(0),
        )
    };
    if done(conn)? {
        return Ok(());
    }
    let tx = super::immediate_transaction(conn)?;
    if done(&tx)? {
        return Ok(());
    }
    tx.execute_batch(
        "CREATE TABLE legacy_imap_keywords (
            account_id TEXT NOT NULL,
            message_id TEXT NOT NULL,
            keywords TEXT NOT NULL,
            PRIMARY KEY (account_id, message_id),
            FOREIGN KEY (account_id, message_id) REFERENCES messages(account_id, message_id)
                ON DELETE CASCADE
        )",
    )?;
    let mut changes = Vec::new();
    {
        let mut stmt = tx.prepare(
            "SELECT m.account_id, m.message_id, m.labels, COALESCE(k.labels, '')
             FROM accounts a JOIN messages m ON m.account_id = a.id
             LEFT JOIN message_keyword_labels k USING (account_id, message_id)
             WHERE a.backend_kind = 'imap' AND (m.labels != '' OR k.labels IS NOT NULL)",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let (labels, flags) = partition_labels(&row.get::<_, String>(2)?);
            let (managed, reserved_managed) = partition_labels(&row.get::<_, String>(3)?);
            if !flags.is_empty() || !reserved_managed.is_empty() {
                changes.push((
                    AccountId(row.get(0)?),
                    row.get::<_, String>(1)?,
                    labels,
                    flags,
                    managed,
                ));
            }
        }
    }
    for (account_id, message_id, labels, mut flags, managed) in changes {
        if !flags.is_empty() {
            // Pre-provenance labels have no reliable copy or revision. Keep
            // that evidence separately so it can't be used as live FLAGS.
            let observed = supported_keywords(&tx, &account_id, &message_id)?;
            flags.retain(|flag| !contains(&observed, flag));
            if !flags.is_empty() {
                tx.execute(
                    "INSERT INTO legacy_imap_keywords VALUES (?1, ?2, ?3)",
                    params![account_id.as_str(), message_id, join_tokens(&flags)],
                )?;
            }
        }
        save_managed(&tx, &account_id, &message_id, &managed)?;
        set_message_labels(&tx, &account_id, &message_id, &join_tokens(&labels))?;
    }
    tx.execute("INSERT INTO meta (key, value) VALUES (?1, '1')", [REPAIR])?;
    tx.commit()
}

fn partition_labels(labels: &str) -> (Vec<String>, Vec<String>) {
    split_field(labels)
        .into_iter()
        .partition(|label| !is_reserved_keyword(label))
}

fn is_imap(conn: &Connection, account_id: &AccountId) -> Result<bool> {
    Ok(conn
        .prepare_cached(
            "SELECT EXISTS(SELECT 1 FROM accounts WHERE id = ?1 AND backend_kind = 'imap')",
        )?
        .query_row([account_id.as_str()], |row| row.get(0))?)
}

pub(crate) fn validate_label(conn: &Connection, account_id: &AccountId, label: &str) -> Result<()> {
    if is_reserved_keyword(label) && is_imap(conn, account_id)? {
        return Err(Error::Invalid(
            "this name is reserved for an IMAP flag".into(),
        ));
    }
    Ok(())
}

/// An undo snapshot made by an older release can still contain flags.
pub(crate) fn restore_visible_labels(
    conn: &Connection,
    account_id: &AccountId,
    encoded: &str,
) -> Result<Option<String>> {
    let (labels, flags) = partition_labels(encoded);
    Ok((!flags.is_empty() && is_imap(conn, account_id)?).then(|| join_tokens(&labels)))
}

/// A label added in Fell stays local even if it had previously been imported
/// from a copy and then taken off in Fell.
pub(super) fn keep_local(
    conn: &Connection,
    account_id: &AccountId,
    message_id: &str,
    visible: &mut Vec<String>,
    label: &str,
) -> Result<()> {
    let mut managed = managed_labels(conn, account_id, message_id)?;
    if contains(&managed, label) {
        visible.retain(|item| {
            item == label || !item.eq_ignore_ascii_case(label) || !managed.contains(item)
        });
        managed.retain(|keyword| !keyword.eq_ignore_ascii_case(label));
        save_managed(conn, account_id, message_id, &managed)?;
    }
    Ok(())
}

const ADDED_LABEL: &str = "added label ";
const REMOVED_LABEL: &str = "removed label ";

#[derive(Clone, Copy)]
pub(crate) enum LabelEdit<'a> {
    Added(&'a str),
    Removed(&'a str),
}

impl<'a> LabelEdit<'a> {
    pub(crate) fn from_summary(summary: &'a str) -> Result<Self> {
        // Both action logs have used these summaries since their first version.
        if let Some(label) = summary
            .strip_prefix(ADDED_LABEL)
            .filter(|label| !label.is_empty())
        {
            Ok(Self::Added(label))
        } else if let Some(label) = summary
            .strip_prefix(REMOVED_LABEL)
            .filter(|label| !label.is_empty())
        {
            Ok(Self::Removed(label))
        } else {
            Err(Error::Action("cannot identify the label for undo".into()))
        }
    }

    pub(crate) fn summary(self) -> String {
        match self {
            Self::Added(label) => format!("{ADDED_LABEL}{label}"),
            Self::Removed(label) => format!("{REMOVED_LABEL}{label}"),
        }
    }

    pub(crate) fn label(self) -> &'a str {
        match self {
            Self::Added(label) | Self::Removed(label) => label,
        }
    }
}

/// Record ownership before adding a local claim or hiding a server label.
pub(crate) fn record_edit(
    conn: &Connection,
    account_id: &AccountId,
    message_id: &str,
    edit: LabelEdit<'_>,
    before: &str,
) -> Result<bool> {
    let mut managed = managed_labels(conn, account_id, message_id)?;
    let LabelEdit::Added(label) = edit else {
        return Ok(managed.iter().any(|keyword| keyword == edit.label()));
    };
    // Re-adding a hidden keyword claims it locally under the spelling the
    // user chose. An already visible chip still owns its exact spelling.
    let visible = split_field(before);
    let previous_count = managed.len();
    managed.retain(|keyword| !keyword.eq_ignore_ascii_case(label) || visible.contains(keyword));
    let was_hidden = managed.len() != previous_count;
    if was_hidden {
        save_managed(conn, account_id, message_id, &managed)?;
    }
    Ok(was_hidden)
}

pub(crate) enum LabelHistory {
    Local,
    Agent,
}

impl LabelHistory {
    fn tables(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Self::Local => ("local_actions", "local_action_messages", "label"),
            Self::Agent => ("agent_actions", "agent_action_messages", "unlabel"),
        }
    }
}

/// While a local re-add is visible it owns the label. A fresh server assertion
/// ends the old suppression, so undo can hand the label back to the server.
/// Check all copies before recording the new FLAGS snapshot.
fn expire_readd_suppression(
    conn: &Connection,
    account_id: &AccountId,
    message_id: &str,
    added: &[String],
) -> Result<()> {
    let added = join_tokens(added).to_ascii_lowercase();
    for history in [LabelHistory::Local, LabelHistory::Agent] {
        let (actions, snapshots, _) = history.tables();
        if !super::copies::table_exists(conn, snapshots)? {
            continue;
        }
        conn.prepare_cached(&format!(
            "UPDATE {snapshots} AS s SET label_was_keyword = 0
             FROM {actions} AS a
             WHERE a.id = s.action_id AND a.account_id = ?1 AND a.undone = 0
               AND a.kind = 'label' AND s.label_was_keyword = 1
               AND (s.message_id = ?2 OR EXISTS (
                   SELECT 1 FROM message_aliases
                   WHERE account_id = ?1 AND message_id = s.message_id AND canonical_message_id = ?2
               ))
               AND substr(a.summary, 1, length(?3)) = ?3
               AND instr(char(30) || ?4 || char(30),
                         char(30) || lower(substr(a.summary, length(?3) + 1)) || char(30)) > 0
               AND NOT EXISTS (
                   SELECT 1 FROM message_copies AS c
                   WHERE c.account_id = ?1 AND c.message_id = ?2
                     AND instr(char(30) || lower(c.keywords) || char(30),
                               char(30) || lower(substr(a.summary, length(?3) + 1)) || char(30)) > 0
               )"
        ))?
        .execute(params![account_id.as_str(), message_id, ADDED_LABEL, added])?;
    }
    Ok(())
}

/// Keep the remaining ownership evidence for removals logged before the
/// snapshot stored it. With no evidence, old labels keep their local meaning.
pub(super) fn upgrade_undo_schema(conn: &Connection) -> Result<()> {
    for history in [LabelHistory::Local, LabelHistory::Agent] {
        let (actions, snapshots, removal_kind) = history.tables();
        let columns = super::column_names(conn, snapshots)?;
        if columns.is_empty() || columns.iter().any(|column| column == "label_was_keyword") {
            continue;
        }
        let tx = super::begin_immediate(conn)?;
        // A concurrent open may have migrated while this one waited.
        if !super::column_names(&tx, snapshots)?
            .iter()
            .any(|column| column == "label_was_keyword")
        {
            tx.execute(
                &format!("ALTER TABLE {snapshots} ADD COLUMN label_was_keyword INTEGER"),
                [],
            )?;
            tx.execute(
                &format!(
                    "UPDATE {snapshots} AS s SET label_was_keyword = 1
                   FROM {actions} AS a
                   JOIN message_keyword_labels AS k
                     ON k.account_id = a.account_id
                   WHERE a.id = s.action_id AND a.undone = 0 AND a.kind = ?1
                     AND k.message_id = COALESCE((
                         SELECT canonical_message_id FROM message_aliases
                         WHERE account_id = a.account_id AND message_id = s.message_id
                     ), s.message_id)
                     AND substr(a.summary, 1, length(?2)) = ?2
                     AND instr(char(30) || k.labels || char(30),
                               char(30) || substr(a.summary, length(?2) + 1) || char(30)) > 0"
                ),
                params![removal_kind, REMOVED_LABEL],
            )?;
        }
        tx.commit()?;
    }
    Ok(())
}

pub(crate) fn undo_label_action(
    conn: &Connection,
    account_id: &AccountId,
    action_id: i64,
    summary: &str,
    history: LabelHistory,
) -> Result<()> {
    let edit = LabelEdit::from_summary(summary)?;
    if is_reserved_keyword(edit.label()) && is_imap(conn, account_id)? {
        return Ok(());
    }
    let outbox_action = match history {
        LabelHistory::Local => action_id,
        LabelHistory::Agent => -action_id,
    };
    let (_, snapshots, _) = history.tables();
    let mut stmt = conn.prepare(&format!(
        "SELECT message_id, labels, label_was_keyword FROM {snapshots}
         WHERE action_id = ?1 ORDER BY message_id"
    ))?;
    let rows = stmt.query_map([action_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<bool>>(2)?,
        ))
    })?;
    for row in rows {
        let (id, before, was_keyword) = row?;
        if let Some((stored, _)) = find_stored(conn, account_id, &MessageId(id.clone()))? {
            if crate::outbox::labels::undo(conn, account_id, outbox_action, &id)? {
                let label = edit.label();
                let mut visible = split_field(&stored.labels);
                if let LabelEdit::Removed(_) = edit {
                    if !visible.iter().any(|name| name == label) {
                        visible.push(label.to_owned());
                    }
                } else {
                    visible.retain(|name| name != label);
                }
                write_labels(conn, account_id, &stored, &join_tokens(&visible))?;
            } else {
                undo_edit(conn, account_id, &stored, edit, &before, was_keyword)?;
            }
        }
    }
    Ok(())
}

pub(crate) fn set_message_labels(
    conn: &Connection,
    account_id: &AccountId,
    message_id: &str,
    labels: &str,
) -> Result<()> {
    if let Some((stored, _)) = find_stored(conn, account_id, &MessageId(message_id.into()))? {
        write_labels(conn, account_id, &stored, labels)?;
    }
    Ok(())
}

fn undo_edit(
    conn: &Connection,
    account_id: &AccountId,
    stored: &Arrival,
    edit: LabelEdit<'_>,
    before: &str,
    was_keyword: Option<bool>,
) -> Result<()> {
    let label = edit.label();
    let before = split_field(before);
    let mut visible = split_field(&stored.labels);
    let mut managed = managed_labels(conn, account_id, &stored.canonical_id)?;
    let is_managed = managed.iter().any(|item| item == label);
    let is_visible = visible.iter().any(|item| item == label);
    let mut keep_managed = is_managed;
    match edit {
        LabelEdit::Added(_) if is_visible || was_keyword == Some(true) => {
            let other_claim = visible
                .iter()
                .chain(&managed)
                .any(|item| item != label && item.eq_ignore_ascii_case(label));
            keep_managed = !other_claim
                && contains(
                    &supported_keywords(conn, account_id, &stored.canonical_id)?,
                    label,
                );
            // Adding locally can have overridden an earlier local removal.
            // Undo restores that hidden state while support is continuous.
            if !keep_managed || (was_keyword == Some(true) && !is_managed) {
                visible.retain(|item| item != label);
            }
        }
        LabelEdit::Removed(_) => {
            let Some(position) = before.iter().position(|item| item == label) else {
                return Ok(());
            };
            let restore_keyword = was_keyword.unwrap_or(is_managed);
            if !is_visible {
                if restore_keyword
                    && (contains(&visible, label)
                        || !contains(
                            &supported_keywords(conn, account_id, &stored.canonical_id)?,
                            label,
                        ))
                {
                    return Ok(());
                }
                keep_managed = restore_keyword;
                let next = before[position + 1..]
                    .iter()
                    .find_map(|item| visible.iter().position(|current| current == item));
                visible.insert(next.unwrap_or(visible.len()), label.to_owned());
            } else if was_keyword == Some(false) {
                keep_managed = false;
            }
        }
        LabelEdit::Added(_) => return Ok(()),
    }
    let previous = managed.clone();
    if keep_managed {
        // A prior undo can have left a hidden marker with a different
        // spelling. Use the spelling of the label this undo restores.
        managed.retain(|item| {
            item == label || !item.eq_ignore_ascii_case(label) || visible.contains(item)
        });
        if !is_managed {
            managed.push(label.to_owned());
        }
    } else if is_managed {
        managed.retain(|item| item != label);
    }
    if managed != previous {
        save_managed(conn, account_id, &stored.canonical_id, &managed)?;
    }
    write_labels(conn, account_id, stored, &join_tokens(&visible))?;
    Ok(())
}

pub(super) fn local_labels(
    conn: &Connection,
    account_id: &AccountId,
    row: &Arrival,
) -> Result<Vec<String>> {
    let managed = managed_labels(conn, account_id, &row.canonical_id)?;
    let mut labels = split_field(&row.labels);
    labels.retain(|label| !managed.contains(label));
    Ok(labels)
}

/// Union the rows' labels in keeper order. Local labels retain their exact
/// spelling and win over any managed keyword with the same IMAP identity.
pub(super) fn merge(
    conn: &Connection,
    account_id: &AccountId,
    from: &Arrival,
    into: &Arrival,
    incoming: &[String],
) -> Result<String> {
    let source = managed_labels(conn, account_id, &from.canonical_id)?;
    let target = managed_labels(conn, account_id, &into.canonical_id)?;
    let from_labels = split_field(&from.labels);
    if source.is_empty() && target.is_empty() {
        let mut incoming = incoming.to_vec();
        incoming.extend(from_labels);
        return Ok(union_tokens(&into.labels, &incoming, false));
    }
    let into_labels = split_field(&into.labels);
    let candidates: Vec<_> = into_labels
        .iter()
        .map(|label| (label, target.contains(label)))
        .chain(
            incoming
                .iter()
                .chain(&from_labels)
                .map(|label| (label, source.contains(label))),
        )
        .collect();
    let local: Vec<_> = candidates
        .iter()
        .filter(|(_, owned)| !owned)
        .map(|(label, _)| (*label).clone())
        .collect();
    let mut visible = Vec::new();
    let mut managed = Vec::new();
    for (label, owned) in candidates {
        if owned {
            if contains(&local, label) || contains(&managed, label) {
                continue;
            }
            managed.push(label.clone());
        }
        visible.push(label.clone());
    }
    // Keep a suppressed keyword if neither copy showed it or claimed it locally.
    for label in target.into_iter().chain(source) {
        if !contains(&local, &label) && !contains(&managed, &label) {
            managed.push(label);
        }
    }
    save_managed(conn, account_id, &into.canonical_id, &managed)?;
    Ok(union_tokens("", &visible, false))
}

fn managed_labels(
    conn: &Connection,
    account_id: &AccountId,
    message_id: &str,
) -> Result<Vec<String>> {
    let labels: Option<String> = conn
        .prepare_cached(
            "SELECT labels FROM message_keyword_labels WHERE account_id = ?1 AND message_id = ?2",
        )?
        .query_row(params![account_id.as_str(), message_id], |row| row.get(0))
        .optional()?;
    Ok(labels
        .map(|labels| split_field(&labels))
        .unwrap_or_default())
}

fn supported_keywords(
    conn: &Connection,
    account_id: &AccountId,
    message_id: &str,
) -> Result<Vec<String>> {
    let mut stmt = conn.prepare_cached(
        "SELECT keywords FROM message_copies
         WHERE account_id = ?1 AND message_id = ?2 AND keywords != ''",
    )?;
    let mut supported = Vec::new();
    for row in stmt.query_map(params![account_id.as_str(), message_id], |row| {
        row.get::<_, String>(0)
    })? {
        supported.extend(split_field(&row?));
    }
    Ok(supported)
}

fn save_managed(
    conn: &Connection,
    account_id: &AccountId,
    message_id: &str,
    labels: &[String],
) -> Result<()> {
    if labels.is_empty() {
        conn.prepare_cached(
            "DELETE FROM message_keyword_labels WHERE account_id = ?1 AND message_id = ?2",
        )?
        .execute(params![account_id.as_str(), message_id])?;
    } else {
        conn.prepare_cached(
            "INSERT INTO message_keyword_labels (account_id, message_id, labels) VALUES (?1, ?2, ?3)
             ON CONFLICT(account_id, message_id) DO UPDATE SET labels = excluded.labels",
        )?.execute(params![account_id.as_str(), message_id, join_tokens(labels)])?;
    }
    Ok(())
}

fn contains(labels: &[String], keyword: &str) -> bool {
    labels
        .iter()
        .any(|label| label.eq_ignore_ascii_case(keyword))
}
