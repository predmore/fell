//! IMAP keyword evidence lives on each server copy. The row remembers only
//! the labels sync introduced, so a server removal cannot take a local label
//! with the same name. Labels on older rows have no provenance and stay local.

use rusqlite::{Connection, OptionalExtension, params};

use crate::Result;
use crate::account::AccountId;
use crate::imap::RemoteCopy;
use crate::message::MessageId;
use crate::search::{IndexedText, sync_terms, terms_for_indexed};

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

pub(crate) fn keep_local_additions(
    conn: &Connection,
    account_id: &AccountId,
    message_id: &str,
    before: &str,
    after: &str,
) -> Result<()> {
    let previous = split_field(before);
    for label in split_field(after)
        .iter()
        .filter(|label| !previous.contains(label))
    {
        keep_local(conn, account_id, message_id, label)?;
    }
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
