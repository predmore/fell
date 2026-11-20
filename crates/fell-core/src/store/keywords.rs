//! IMAP keyword evidence lives on each server copy. The row remembers only
//! the labels sync introduced, so a server removal cannot take a local label
//! with the same name. Labels on older rows have no provenance and stay local.

use rusqlite::{Connection, OptionalExtension, params};

use crate::account::AccountId;
use crate::imap::RemoteCopy;
use crate::message::MessageId;
use crate::search::{IndexedText, sync_terms, terms_for_indexed};
use crate::{Error, Result};

use super::{Arrival, find_stored, join_tokens, split_field};

pub(super) fn observe(
    conn: &Connection,
    account_id: &AccountId,
    copy: &RemoteCopy,
    modseq: Option<u64>,
    keywords: &[String],
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
    let removed = before.iter().any(|keyword| !contains(&unique, keyword));
    let added: Vec<_> = unique
        .into_iter()
        .filter(|keyword| !contains(&before, keyword))
        .collect();
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

/// A label added in Fell stays local even if it had previously been imported
/// from a copy and then taken off in Fell.
pub(super) fn keep_local(
    conn: &Connection,
    account_id: &AccountId,
    message_id: &str,
    label: &str,
) -> Result<()> {
    let mut managed = managed_labels(conn, account_id, message_id)?;
    if managed.iter().any(|keyword| keyword == label) {
        managed.retain(|keyword| keyword != label);
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

    fn label(self) -> &'a str {
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
) -> Result<bool> {
    let mut managed = managed_labels(conn, account_id, message_id)?;
    let was_keyword = managed.iter().any(|label| label == edit.label());
    if let LabelEdit::Added(label) = edit
        && was_keyword
    {
        managed.retain(|keyword| keyword != label);
        save_managed(conn, account_id, message_id, &managed)?;
    }
    Ok(was_keyword)
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
        if let Some((stored, _)) = find_stored(conn, account_id, &MessageId(id))? {
            undo_edit(conn, account_id, &stored, edit, &before, was_keyword)?;
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
            // Undo restores that hidden state while the copies still support it.
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
    if keep_managed != is_managed {
        managed.retain(|item| item != label);
        if keep_managed {
            managed.push(label.to_owned());
        }
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

/// The duplicate merge unions the visible labels and repoints the copies.
/// A label that was local on either row keeps that ownership.
pub(super) fn merge(
    conn: &Connection,
    account_id: &AccountId,
    from: &Arrival,
    into: &Arrival,
) -> Result<()> {
    let source = managed_labels(conn, account_id, &from.canonical_id)?;
    let mut target = managed_labels(conn, account_id, &into.canonical_id)?;
    if source.is_empty() && target.is_empty() {
        return Ok(());
    }
    let from_labels = split_field(&from.labels);
    let into_labels = split_field(&into.labels);
    target.retain(|label| !from_labels.contains(label) || source.contains(label));
    for label in &source {
        if !into_labels.contains(label) && !target.contains(label) {
            target.push(label.clone());
        }
    }
    save_managed(conn, account_id, &into.canonical_id, &target)?;
    Ok(())
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
