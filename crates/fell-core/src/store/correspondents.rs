//! Recipient completion is derived from canonical messages and confirmed local
//! sends. A message contributes at most one sighting per address, even when
//! multiple folders or the local send and its Sent copy report it.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{Connection, OptionalExtension, params};
use unicode_properties::{GeneralCategory, UnicodeGeneralCategory};

use super::{Store, begin_immediate, immediate_transaction, roles, split_field};
use crate::message::{Address, MessageId, decode_addresses, encode_addresses};
use crate::{AccountId, FolderRole, FolderRoles, Result};

const VERSION: &str = "correspondents_v1";
const BACKFILL: &str = "correspondents_next_row";
pub(super) const WRITE_BATCH: usize = 32;
const MAINTENANCE_BATCH: usize = 128;
const PREFIX_CHARS: usize = 24;
const LIMIT: usize = 6;

// The queue exists before store upgrades that use WriteTx. Its producers
// are installed with the index, after the older upgrades have finished.
pub(super) const QUEUE: &str = "
CREATE TABLE IF NOT EXISTS correspondent_dirty (
    account_id TEXT NOT NULL,
    message_id TEXT NOT NULL,
    PRIMARY KEY (account_id, message_id)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS correspondent_sends (
    account_id TEXT NOT NULL,
    message_id TEXT NOT NULL,
    recipients TEXT NOT NULL,
    date_unix INTEGER NOT NULL,
    PRIMARY KEY (account_id, message_id)
) WITHOUT ROWID;";

const SCHEMA: &str = "
CREATE TABLE correspondents (
    account_id TEXT NOT NULL,
    address TEXT NOT NULL,
    email TEXT NOT NULL,
    name TEXT,
    last_contacted INTEGER NOT NULL,
    sent_to INTEGER NOT NULL,
    received_from INTEGER NOT NULL,
    ordinary INTEGER NOT NULL,
    PRIMARY KEY (account_id, address)
) WITHOUT ROWID;

CREATE TABLE correspondent_sightings (
    account_id TEXT NOT NULL,
    message_id TEXT NOT NULL,
    address TEXT NOT NULL,
    email TEXT NOT NULL,
    name TEXT,
    name_quality INTEGER NOT NULL,
    date_unix INTEGER NOT NULL,
    sent INTEGER NOT NULL,
    received INTEGER NOT NULL,
    list_mail INTEGER NOT NULL,
    PRIMARY KEY (account_id, message_id, address)
) WITHOUT ROWID;
CREATE INDEX correspondent_sightings_name ON correspondent_sightings
    (account_id, address, name_quality DESC, date_unix DESC, name DESC, email DESC);
CREATE INDEX correspondent_sightings_date ON correspondent_sightings
    (account_id, address, date_unix DESC);

-- A prefix seek walks only the first six rows in ranking order, even when
-- every correspondent matches. A contact appears once for any given prefix.
CREATE TABLE correspondent_prefixes (
    account_id TEXT NOT NULL,
    prefix TEXT NOT NULL,
    automated INTEGER NOT NULL,
    outbound INTEGER NOT NULL,
    score INTEGER NOT NULL,
    address TEXT NOT NULL,
    PRIMARY KEY (account_id, prefix, automated, outbound DESC, score DESC, address)
) WITHOUT ROWID;

CREATE TRIGGER correspondents_message_insert AFTER INSERT ON messages BEGIN
    INSERT OR IGNORE INTO correspondent_dirty VALUES (new.account_id, new.message_id);
END;
CREATE TRIGGER correspondents_message_delete AFTER DELETE ON messages BEGIN
    INSERT OR IGNORE INTO correspondent_dirty VALUES (old.account_id, old.message_id);
END;
CREATE TRIGGER correspondents_message_change
AFTER UPDATE OF from_name, from_email, date_unix, folders, list_id, to_line, cc_line, bcc_line ON messages
WHEN old.from_name IS NOT new.from_name OR old.from_email != new.from_email
    OR old.date_unix != new.date_unix OR old.folders != new.folders
    OR old.list_id IS NOT new.list_id OR old.to_line != new.to_line
    OR old.cc_line != new.cc_line OR old.bcc_line != new.bcc_line
BEGIN
    INSERT OR IGNORE INTO correspondent_dirty VALUES (new.account_id, new.message_id);
END;
CREATE TRIGGER correspondents_alias_insert AFTER INSERT ON message_aliases BEGIN
    INSERT OR IGNORE INTO correspondent_dirty VALUES (new.account_id, new.message_id);
    INSERT OR IGNORE INTO correspondent_dirty VALUES (new.account_id, new.canonical_message_id);
END;
CREATE TRIGGER correspondents_alias_delete AFTER DELETE ON message_aliases BEGIN
    INSERT OR IGNORE INTO correspondent_dirty VALUES (old.account_id, old.message_id);
    INSERT OR IGNORE INTO correspondent_dirty VALUES (old.account_id, old.canonical_message_id);
END;
CREATE TRIGGER correspondents_alias_change AFTER UPDATE OF canonical_message_id ON message_aliases
WHEN old.canonical_message_id != new.canonical_message_id BEGIN
    INSERT OR IGNORE INTO correspondent_dirty VALUES (old.account_id, old.canonical_message_id);
    INSERT OR IGNORE INTO correspondent_dirty VALUES (new.account_id, new.canonical_message_id);
END;";

pub(super) fn ensure(conn: &Connection) -> Result<()> {
    let installed = |conn: &Connection| {
        conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM meta WHERE key = ?1)",
            [VERSION],
            |row| row.get::<_, bool>(0),
        )
    };
    if installed(conn)? {
        return Ok(());
    }
    let tx = begin_immediate(conn)?;
    if !installed(&tx)? {
        tx.execute_batch(SCHEMA)?;
        tx.execute(
            "INSERT INTO meta SELECT ?1, rowid FROM messages ORDER BY rowid LIMIT 1",
            [BACKFILL],
        )?;
        tx.execute("INSERT INTO meta VALUES (?1, '1')", [VERSION])?;
    }
    tx.commit()?;
    Ok(())
}

/// A changed server folder mapping can reclassify mail already stored in it.
pub(super) fn roles_changed(
    conn: &Connection,
    account: &AccountId,
    found: &FolderRoles,
) -> Result<()> {
    let previous = roles::listed(conn, account)?.unwrap_or_default();
    let relevant = [
        FolderRole::Sent,
        FolderRole::Junk,
        FolderRole::Drafts,
        FolderRole::All,
    ];
    if relevant.into_iter().any(|role| {
        previous.get(role).map(|folder| &folder.name) != found.get(role).map(|folder| &folder.name)
    }) {
        conn.execute(
            "INSERT OR IGNORE INTO correspondent_dirty
             SELECT account_id, message_id FROM messages WHERE account_id = ?1",
            [account.as_str()],
        )?;
    }
    Ok(())
}

pub(super) fn dirty(conn: &Connection, account: &AccountId, id: &MessageId) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO correspondent_dirty
         VALUES (?1, coalesce((SELECT canonical_message_id FROM message_aliases
                               WHERE account_id = ?1 AND message_id = ?2), ?2))",
        params![account.as_str(), id.as_str()],
    )?;
    Ok(())
}

pub(crate) fn forget_send(conn: &Connection, account: &AccountId, id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM correspondent_sends
         WHERE account_id = ?1 AND message_id IN (
             SELECT ?2 UNION ALL SELECT message_id FROM message_aliases
             WHERE account_id = ?1 AND canonical_message_id = ?2
         )",
        params![account.as_str(), id],
    )?;
    Ok(())
}

impl Store {
    /// Bring one batch of correspondent history up to date. The bootstrap
    /// cursor and the index advance together so a restart can resume here.
    /// Returns whether to try another batch before idling.
    pub fn maintain_correspondents(&self) -> Result<bool> {
        let pending = self
            .conn
            .prepare_cached(
                "SELECT EXISTS (SELECT 1 FROM correspondent_dirty)
                 OR EXISTS (SELECT 1 FROM meta WHERE key = ?1)",
            )?
            .query_row([BACKFILL], |row| row.get::<_, bool>(0))?;
        if !pending {
            return Ok(false);
        }
        let tx = begin_immediate(&self.conn)?;
        let refreshed = refresh_dirty(&tx, MAINTENANCE_BATCH)?;
        let more = if refreshed == MAINTENANCE_BATCH {
            true
        } else {
            let remaining = MAINTENANCE_BATCH - refreshed;
            queue_backfill(&tx, remaining)?;
            refresh_dirty(&tx, remaining)? == remaining
        };
        tx.commit()?;
        Ok(more)
    }

    pub(crate) fn record_sent_correspondents(
        &self,
        account: &AccountId,
        message_id: &MessageId,
        recipients: &[Address],
        date_unix: i64,
    ) -> Result<()> {
        let tx = immediate_transaction(&self.conn)?;
        if crate::actions::message_was_purged(&tx, account, message_id.as_str())? {
            return tx.commit();
        }
        tx.execute(
            "INSERT INTO correspondent_sends VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (account_id, message_id) DO NOTHING",
            params![
                account.as_str(),
                message_id.as_str(),
                encode_addresses(recipients),
                date_unix
            ],
        )?;
        dirty(&tx, account, message_id)?;
        tx.commit()
    }

    /// The best matches in this account's local mail, in their display order.
    pub fn correspondent_suggestions(
        &self,
        account: &AccountId,
        prefix: &str,
    ) -> Result<Vec<Address>> {
        suggest(&self.conn, account, prefix, LIMIT)
    }
}

pub(super) fn suggest(
    conn: &Connection,
    account: &AccountId,
    prefix: &str,
    limit: usize,
) -> Result<Vec<Address>> {
    let prefix = fold(prefix);
    if prefix.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    let indexed_end = prefix.char_indices().nth(PREFIX_CHARS).map(|(i, _)| i);
    let indexed = &prefix[..indexed_end.unwrap_or(prefix.len())];
    let mut select = conn.prepare_cached(
        "SELECT c.email, c.name FROM correspondent_prefixes p
         CROSS JOIN correspondents c ON c.account_id = p.account_id AND c.address = p.address
         WHERE p.account_id = ?1 AND p.prefix = ?2
         ORDER BY p.automated, p.outbound DESC, p.score DESC, p.address",
    )?;
    let mut rows = select.query(params![account.as_str(), indexed])?;
    let mut result = Vec::with_capacity(limit.min(LIMIT));
    while result.len() < limit.min(LIMIT) {
        let Some(row) = rows.next()? else { break };
        let person = Address {
            email: row.get(0)?,
            name: row.get(1)?,
        };
        if safe_email(&person.email)
            && !person.email.chars().any(char::is_whitespace)
            && (indexed_end.is_none()
                || terms(&person).iter().any(|term| term.starts_with(&prefix)))
        {
            result.push(person);
        }
    }
    Ok(result)
}

#[derive(PartialEq, Eq)]
struct Sighting {
    person: Address,
    date: i64,
    quality: i64,
    sent: bool,
    received: bool,
    list: bool,
}

type Sightings = BTreeMap<String, Sighting>;

#[derive(Default, PartialEq, Eq)]
struct Counts {
    sent: i64,
    received: i64,
    ordinary: i64,
}

impl Counts {
    fn adjust(&mut self, seen: &Sighting, sign: i64) {
        self.sent += sign * i64::from(seen.sent);
        self.received += sign * i64::from(seen.received);
        self.ordinary += sign * i64::from(!seen.list);
    }

    fn apply(&mut self, delta: &Self) {
        self.sent += delta.sent;
        self.received += delta.received;
        self.ordinary += delta.ordinary;
    }
}

struct Mailbox {
    own: String,
    roles: FolderRoles,
}

fn queue_backfill(conn: &Connection, limit: usize) -> Result<()> {
    let next = conn
        .prepare_cached("SELECT CAST(value AS INTEGER) FROM meta WHERE key = ?1")?
        .query_row([BACKFILL], |row| row.get::<_, i64>(0))
        .optional()?;
    let Some(next) = next else { return Ok(()) };
    let batch = conn
        .prepare_cached(
            "SELECT rowid, account_id, message_id FROM messages
         WHERE rowid >= ?1 ORDER BY rowid LIMIT ?2",
        )?
        .query_map(params![next, limit as i64], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut enqueue =
        conn.prepare_cached("INSERT OR IGNORE INTO correspondent_dirty VALUES (?1, ?2)")?;
    for (_, account, id) in &batch {
        enqueue.execute(params![account, id])?;
    }
    if batch.len() == limit
        && let Some(next) = batch.last().and_then(|(rowid, _, _)| rowid.checked_add(1))
    {
        conn.execute(
            "UPDATE meta SET value = ?2 WHERE key = ?1",
            params![BACKFILL, next],
        )?;
    } else {
        conn.execute("DELETE FROM meta WHERE key = ?1", [BACKFILL])?;
    }
    Ok(())
}

pub(super) fn refresh_dirty(conn: &Connection, limit: usize) -> Result<usize> {
    let queue = conn.prepare_cached(
        "SELECT account_id, message_id FROM correspondent_dirty ORDER BY account_id, message_id LIMIT ?1",
    )?.query_map([limit as i64], |row| Ok((AccountId(row.get(0)?), row.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let count = queue.len();
    let mut dequeue = conn.prepare_cached(
        "DELETE FROM correspondent_dirty WHERE account_id = ?1 AND message_id = ?2",
    )?;
    let mut queue = queue.as_slice();
    while let Some(((account, _), _)) = queue.split_first() {
        let end = queue.partition_point(|(id, _)| id == account);
        let mailbox = Mailbox {
            own: conn
                .query_row(
                    "SELECT email FROM accounts WHERE id = ?1",
                    [account.as_str()],
                    |row| row.get::<_, String>(0),
                )
                .optional()?
                .unwrap_or_default()
                .trim()
                .to_lowercase(),
            roles: roles::listed(conn, account)?.unwrap_or_default(),
        };
        let mut affected = BTreeMap::new();
        for (_, id) in &queue[..end] {
            refresh_message(conn, account, id, &mailbox, &mut affected)?;
            dequeue.execute(params![account.as_str(), id])?;
        }
        for (address, counts) in affected {
            refresh_contact(conn, account, &address, &counts)?;
        }
        queue = &queue[end..];
    }
    Ok(count)
}

fn refresh_message(
    conn: &Connection,
    account: &AccountId,
    id: &str,
    mailbox: &Mailbox,
    affected: &mut BTreeMap<String, Counts>,
) -> Result<()> {
    let current = message_sightings(conn, account, id, mailbox)?;
    let old = conn
        .prepare_cached(
            "SELECT address, email, name, date_unix, name_quality, sent, received, list_mail
         FROM correspondent_sightings WHERE account_id = ?1 AND message_id = ?2",
        )?
        .query_map(params![account.as_str(), id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                Sighting {
                    person: Address {
                        email: row.get(1)?,
                        name: row.get(2)?,
                    },
                    date: row.get(3)?,
                    quality: row.get(4)?,
                    sent: row.get(5)?,
                    received: row.get(6)?,
                    list: row.get(7)?,
                },
            ))
        })?
        .collect::<rusqlite::Result<Sightings>>()?;
    for (address, prior) in old
        .iter()
        .filter(|(address, _)| !current.contains_key(*address))
    {
        conn.prepare_cached(
            "DELETE FROM correspondent_sightings WHERE account_id = ?1 AND message_id = ?2 AND address = ?3",
        )?.execute(params![account.as_str(), id, address])?;
        affected
            .entry(address.clone())
            .or_default()
            .adjust(prior, -1);
    }
    for (address, seen) in current {
        if old.get(&address) == Some(&seen) {
            continue;
        }
        conn.prepare_cached(
            "INSERT OR REPLACE INTO correspondent_sightings VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        )?.execute(params![account.as_str(), id, address, seen.person.email, seen.person.name,
            seen.quality, seen.date, seen.sent, seen.received, seen.list])?;
        let counts = affected.entry(address.clone()).or_default();
        if let Some(prior) = old.get(&address) {
            counts.adjust(prior, -1);
        }
        counts.adjust(&seen, 1);
    }
    Ok(())
}

fn message_sightings(
    conn: &Connection,
    account: &AccountId,
    id: &str,
    mailbox: &Mailbox,
) -> Result<Sightings> {
    let mut seen = Sightings::new();
    let mut mail = conn.prepare_cached(
        "SELECT from_email, from_name, date_unix, folders, list_id IS NOT NULL,
                to_line, cc_line, bcc_line
         FROM messages WHERE account_id = ?1 AND message_id = ?2",
    )?;
    let mut row = mail.query(params![account.as_str(), id])?;
    let mut stored_sent = None;
    if let Some(row) = row.next()? {
        let folders = split_field(&row.get::<_, String>(3)?);
        let date = row.get::<_, i64>(2)?;
        let sent = folders
            .iter()
            .any(|folder| is_folder(mailbox, FolderRole::Sent, folder));
        let junk = folders
            .iter()
            .any(|folder| is_folder(mailbox, FolderRole::Junk, folder));
        let normal = folders.iter().any(|folder| {
            !is_folder(mailbox, FolderRole::Junk, folder)
                && !is_folder(mailbox, FolderRole::Drafts, folder)
                && (!junk || !is_folder(mailbox, FolderRole::All, folder))
        });
        if sent {
            stored_sent = Some(date);
        } else if normal || folders.is_empty() {
            add(
                &mut seen,
                mailbox,
                Address {
                    email: row.get(0)?,
                    name: row.get(1)?,
                },
                date,
                Direction::Received { list: row.get(4)? },
            );
        }
        if sent || normal || folders.is_empty() {
            for column in 5..=if sent { 7 } else { 6 } {
                for person in decode_addresses(&row.get::<_, String>(column)?) {
                    add(
                        &mut seen,
                        mailbox,
                        person,
                        date,
                        if sent {
                            Direction::Sent
                        } else {
                            Direction::Mentioned
                        },
                    );
                }
            }
        }
    }
    // Aliases use the canonical row's contribution. Their own old
    // contribution is removed when a late Sent copy collapses into an echo.
    let alias = conn
        .prepare_cached("SELECT 1 FROM message_aliases WHERE account_id = ?1 AND message_id = ?2")?
        .exists(params![account.as_str(), id])?;
    if alias {
        return Ok(Sightings::new());
    }
    if let Some(date) = stored_sent {
        // The Sent copy and the separately recorded Bcc now own this
        // sighting, including when the Sent id was merged into a list echo.
        forget_send(conn, account, id)?;
        let mut legacy = conn.prepare_cached(
            "SELECT bcc_addrs FROM sent_bcc
             WHERE account_id = ?1 AND message_id IN (
                SELECT ?2 UNION ALL SELECT message_id FROM message_aliases
                WHERE account_id = ?1 AND canonical_message_id = ?2
             )",
        )?;
        let mut rows = legacy.query(params![account.as_str(), id])?;
        while let Some(row) = rows.next()? {
            for person in decode_addresses(&row.get::<_, String>(0)?) {
                add(&mut seen, mailbox, person, date, Direction::Sent);
            }
        }
    } else {
        let mut local = conn.prepare_cached(
            "SELECT recipients, date_unix FROM correspondent_sends
             WHERE account_id = ?1 AND message_id IN (
                 SELECT ?2 UNION ALL SELECT message_id FROM message_aliases
                 WHERE account_id = ?1 AND canonical_message_id = ?2
             )",
        )?;
        let mut rows = local.query(params![account.as_str(), id])?;
        while let Some(row) = rows.next()? {
            for person in decode_addresses(&row.get::<_, String>(0)?) {
                add(&mut seen, mailbox, person, row.get(1)?, Direction::Sent);
            }
        }
    }
    Ok(seen)
}

fn is_folder(mailbox: &Mailbox, role: FolderRole, folder: &str) -> bool {
    mailbox.roles.get(role).map_or_else(
        || {
            let leaf = folder
                .split_once('/')
                .filter(|(head, _)| {
                    head.eq_ignore_ascii_case("INBOX") || head.eq_ignore_ascii_case("[Gmail]")
                })
                .map_or(folder, |(_, leaf)| leaf);
            role.fallback_names()
                .iter()
                .any(|name| name.eq_ignore_ascii_case(leaf))
        },
        |known| known.name.eq_ignore_ascii_case(folder),
    )
}

#[derive(Clone, Copy)]
enum Direction {
    Sent,
    Received { list: bool },
    Mentioned,
}

fn safe_email(email: &str) -> bool {
    !email.chars().any(|ch| {
        ch.is_control()
            || (!ch.is_ascii()
                && (ch.general_category() == GeneralCategory::Format
                // Reserved in the bidi formatting range and stripped by display_safe.
                || ch == '\u{2065}'))
    })
}

fn add(
    seen: &mut Sightings,
    mailbox: &Mailbox,
    mut person: Address,
    date: i64,
    direction: Direction,
) {
    if !safe_email(&person.email) {
        return;
    }
    person.email = person.email.trim().to_owned();
    let address = person.email.to_lowercase();
    if address == mailbox.own
        || !address.contains('@')
        || address.chars().any(char::is_whitespace)
        || address.contains(['<', '>'])
    {
        return;
    }
    person.name = person.name.take().and_then(|name| {
        let name = name.trim();
        (!name.is_empty() && name.to_lowercase() != address).then(|| name.to_owned())
    });
    let sent = matches!(direction, Direction::Sent);
    let received = matches!(direction, Direction::Received { .. });
    let list = matches!(direction, Direction::Received { list: true });
    let quality = person.name.as_deref().map_or(0, |name| {
        let full = name.split_whitespace().nth(1).is_some();
        1 + 2 * i64::from(full) + i64::from(received && !list)
    });
    let incoming = (quality, date, &person.name, &person.email);
    if let Some(prior) = seen.get_mut(&address) {
        if incoming
            > (
                prior.quality,
                prior.date,
                &prior.person.name,
                &prior.person.email,
            )
        {
            prior.person = person;
            prior.quality = quality;
        }
        prior.date = prior.date.max(date);
        prior.sent |= sent;
        prior.received |= received;
        prior.list = (prior.list || list) && !prior.sent;
    } else {
        seen.insert(
            address,
            Sighting {
                person,
                date,
                quality,
                sent,
                received,
                list,
            },
        );
    }
}

#[derive(PartialEq, Eq)]
struct Contact {
    person: Address,
    last: i64,
    counts: Counts,
}

impl Contact {
    fn rank(&self, address: &str) -> (bool, bool, i64) {
        // Frequency helps a regular contact stay ahead of a one-off.
        // Each message adds a day's worth of recency, up to one month.
        let score = self
            .last
            .saturating_add((self.counts.sent * 2 + self.counts.received).min(30) * 86_400);
        (
            self.counts.ordinary == 0 || no_reply(address),
            self.counts.sent > 0,
            score,
        )
    }
}

fn refresh_contact(
    conn: &Connection,
    account: &AccountId,
    address: &str,
    delta: &Counts,
) -> Result<()> {
    let key = params![account.as_str(), address];
    let old = conn
        .prepare_cached(
            "SELECT email, name, last_contacted, sent_to, received_from, ordinary
         FROM correspondents WHERE account_id = ?1 AND address = ?2",
        )?
        .query_row(key, |row| {
            Ok(Contact {
                person: Address {
                    email: row.get(0)?,
                    name: row.get(1)?,
                },
                last: row.get(2)?,
                counts: Counts {
                    sent: row.get(3)?,
                    received: row.get(4)?,
                    ordinary: row.get(5)?,
                },
            })
        })
        .optional()?;
    let new = conn
        .prepare_cached(
            "SELECT email, name,
                    (SELECT date_unix FROM correspondent_sightings
                     WHERE account_id = ?1 AND address = ?2 ORDER BY date_unix DESC LIMIT 1)
             FROM correspondent_sightings WHERE account_id = ?1 AND address = ?2
             ORDER BY name_quality DESC, date_unix DESC, name DESC, email DESC LIMIT 1",
        )?
        .query_row(key, |row| {
            let mut counts = Counts::default();
            if let Some(old) = &old {
                counts.apply(&old.counts);
            }
            counts.apply(delta);
            Ok(Contact {
                person: Address {
                    email: row.get(0)?,
                    name: row.get(1)?,
                },
                last: row.get(2)?,
                counts,
            })
        })
        .optional()?;
    if old == new {
        return Ok(());
    }
    if old.as_ref().map(|c| (&c.person.name, c.rank(address)))
        != new.as_ref().map(|c| (&c.person.name, c.rank(address)))
    {
        if let Some(old) = &old {
            write_prefixes(conn, account, address, old, false)?;
        }
        if let Some(new) = &new {
            write_prefixes(conn, account, address, new, true)?;
        }
    }
    if let Some(contact) = new {
        conn.prepare_cached(
            "INSERT OR REPLACE INTO correspondents VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )?
        .execute(params![
            account.as_str(),
            address,
            contact.person.email,
            contact.person.name,
            contact.last,
            contact.counts.sent,
            contact.counts.received,
            contact.counts.ordinary
        ])?;
    } else {
        conn.prepare_cached("DELETE FROM correspondents WHERE account_id = ?1 AND address = ?2")?
            .execute(key)?;
    }
    Ok(())
}

fn write_prefixes(
    conn: &Connection,
    account: &AccountId,
    address: &str,
    contact: &Contact,
    insert: bool,
) -> Result<()> {
    let mut stmt = conn.prepare_cached(if insert {
        "INSERT INTO correspondent_prefixes VALUES (?1, ?2, ?3, ?4, ?5, ?6)"
    } else {
        "DELETE FROM correspondent_prefixes WHERE account_id = ?1 AND prefix = ?2
         AND automated = ?3 AND outbound = ?4 AND score = ?5 AND address = ?6"
    })?;
    let mut prefixes = BTreeSet::new();
    let terms = terms(&contact.person);
    for term in &terms {
        prefixes.extend(
            term.char_indices()
                .skip(1)
                .map(|(i, _)| &term[..i])
                .chain(std::iter::once(term.as_str()))
                .take(PREFIX_CHARS),
        );
    }
    let (automated, outbound, score) = contact.rank(address);
    for prefix in prefixes {
        stmt.execute(params![
            account.as_str(),
            prefix,
            automated,
            outbound,
            score,
            address
        ])?;
    }
    Ok(())
}

fn no_reply(address: &str) -> bool {
    let local = address.split_once('@').map_or(address, |(local, _)| local);
    let compact: String = local
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    compact.starts_with("noreply") || compact.starts_with("donotreply")
}

fn fold(text: &str) -> String {
    text.split_whitespace()
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

fn terms(person: &Address) -> Vec<String> {
    let mut terms = vec![person.email.to_lowercase()];
    if let Some(name) = &person.name {
        let name = fold(name);
        let words: Vec<_> = name.split_whitespace().collect();
        let initials: String = words
            .iter()
            .filter_map(|word| word.chars().next())
            .collect();
        terms.push(initials);
        terms.extend(words.iter().skip(1).map(|word| (*word).to_owned()));
        terms.push(name);
    }
    terms
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    fn person(name: &str, email: &str) -> Address {
        Address {
            name: Some(name.into()),
            email: email.into(),
        }
    }

    #[test]
    fn upgrade_backfills_and_reopen_keeps_counts() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("mail.db");
        let legacy = Connection::open(&path).unwrap();
        legacy.execute_batch(super::super::SCHEMA).unwrap();
        legacy
            .execute_batch(
                "INSERT INTO accounts VALUES ('a', 'Demo', 'me@example.test', 'bridge');
             INSERT INTO messages (account_id, message_id, remote_id, thread_id, references_header,
                         from_name, from_email, subject, date_unix, snippet, folders)
             VALUES ('a', 'one', 'remote:one', 'one', '', 'Élodie Martin', 'elodie@example.test',
                     'Notes', 210, '', 'INBOX');",
            )
            .unwrap();
        drop(legacy);
        for _ in 0..2 {
            let store = Store::open(&path).unwrap();
            while store.maintain_correspondents().unwrap() {}
            for prefix in ["ÉLO", "ÉM", "mart", "elodie@"] {
                assert_eq!(
                    store
                        .correspondent_suggestions(&AccountId("a".into()), prefix)
                        .unwrap(),
                    [person("Élodie Martin", "elodie@example.test")]
                );
            }
            let count: i64 = store
                .conn
                .query_row(
                    "SELECT received_from FROM correspondents WHERE account_id = 'a'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, 1);
        }
    }

    #[test]
    fn large_upgrade_opens_before_backfilling_and_resumes_in_batches() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("mail.db");
        let store = Store::open(&path).unwrap();
        let tx = super::super::immediate_transaction(&store.conn).unwrap();
        tx.execute_batch(
            "INSERT INTO accounts VALUES ('a', 'Demo', 'me@example.test', 'bridge');
             INSERT INTO messages (account_id, message_id, remote_id, thread_id, references_header,
                         from_name, from_email, subject, date_unix, snippet, folders)
             WITH RECURSIVE counter(i) AS (VALUES (1) UNION ALL SELECT i + 1 FROM counter WHERE i < 100000)
             SELECT 'a', 'msg:' || i, 'remote:' || i, 'one', '',
                    'Demo Sender', 'demo@example.test', 'Notes', 210, '', 'INBOX' FROM counter;
             INSERT INTO search_terms VALUES ('a', 'notes', 100000);",
        ).unwrap();
        tx.commit().unwrap();
        let count = |conn: &Connection, sql: &str| {
            conn.query_row(sql, [], |row| row.get::<_, u32>(0)).unwrap() as usize
        };
        assert_eq!(
            count(&store.conn, "SELECT COUNT(*) FROM correspondent_dirty"),
            100_000 - WRITE_BATCH
        );
        // Leave a current mailbox with only the autocomplete schema absent.
        store
            .conn
            .execute_batch(
                "DROP TRIGGER correspondents_message_insert;
             DROP TRIGGER correspondents_message_delete;
             DROP TRIGGER correspondents_message_change;
             DROP TRIGGER correspondents_alias_insert;
             DROP TRIGGER correspondents_alias_delete;
             DROP TRIGGER correspondents_alias_change;
             DROP TABLE correspondent_sightings;
             DROP TABLE correspondent_prefixes;
             DROP TABLE correspondents;
             DELETE FROM correspondent_dirty;
             DELETE FROM meta WHERE key = 'correspondents_v1';",
            )
            .unwrap();
        drop(store);
        let started = Instant::now();
        let store = Store::open(&path).unwrap();
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_millis(300),
            "store open took {elapsed:?}"
        );
        let sightings = "SELECT COUNT(*) FROM correspondent_sightings";
        assert_eq!(count(&store.conn, sightings), 0);
        assert!(store.maintain_correspondents().unwrap());
        assert_eq!(count(&store.conn, sightings), MAINTENANCE_BATCH);
        drop(store);
        let store = Store::open(&path).unwrap();
        assert!(store.maintain_correspondents().unwrap());
        assert_eq!(count(&store.conn, sightings), MAINTENANCE_BATCH * 2);
    }

    #[test]
    fn a_list_sender_mentioned_in_its_own_mail_still_ranks_below_other_contacts() {
        let store = Store::open_in_memory().unwrap();
        let account = AccountId("a".into());
        let tx = immediate_transaction(&store.conn).unwrap();
        tx.execute_batch(
            "INSERT INTO accounts VALUES ('a', 'Demo', 'me@example.test', 'bridge');
             INSERT INTO messages (account_id, message_id, remote_id, thread_id, references_header,
                         from_name, from_email, subject, date_unix, snippet, folders, list_id)
             VALUES ('a', 'list', 'remote:list', 'list', '', 'Demo Digest', 'digest@example.test',
                     'Digest', 800000, '', 'INBOX', 'list.example.test'),
                    ('a', 'human', 'remote:human', 'human', '', 'Demo Sender', 'demo@example.test',
                     'Notes', 210, '', 'INBOX', NULL);",
        )
        .unwrap();
        tx.execute(
            "UPDATE messages SET to_line = ?1 WHERE message_id = 'list'",
            [encode_addresses(&[person(
                "Demo Digest",
                "digest@example.test",
            )])],
        )
        .unwrap();
        tx.commit().unwrap();
        assert_eq!(
            store.correspondent_suggestions(&account, "demo").unwrap(),
            [
                person("Demo Sender", "demo@example.test"),
                person("Demo Digest", "digest@example.test")
            ],
        );
    }

    #[test]
    fn addresses_with_controls_or_format_characters_never_enter_the_index() {
        let store = Store::open_in_memory().unwrap();
        let account = AccountId("a".into());
        let tx = immediate_transaction(&store.conn).unwrap();
        tx.execute(
            "INSERT INTO accounts VALUES ('a', 'Demo', 'me@example.test', 'bridge')",
            [],
        )
        .unwrap();
        for (i, mark) in [
            '\n',
            '\u{00ad}',
            '\u{061c}',
            '\u{200d}',
            '\u{202e}',
            '\u{2065}',
            '\u{e0001}',
        ]
        .into_iter()
        .enumerate()
        {
            tx.execute(
                "INSERT INTO messages (account_id, message_id, remote_id, thread_id, references_header,
                            from_name, from_email, subject, date_unix, snippet, folders)
                 VALUES ('a', ?1, ?1, ?1, '', 'Demo Sender', ?2, 'Notes', 210, '', 'INBOX')",
                params![i.to_string(), format!("demo{mark}@example.test")],
            ).unwrap();
        }
        tx.commit().unwrap();
        assert!(
            store
                .correspondent_suggestions(&account, "demo")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn ranked_prefixes_at_twenty_thousand_correspondents() {
        let store = Store::open_in_memory().unwrap();
        let account = AccountId("a".into());
        let tx = begin_immediate(&store.conn).unwrap();
        let mut insert = tx
            .prepare("INSERT INTO correspondents VALUES ('a', ?1, ?1, ?2, ?3, 0, 1, 1)")
            .unwrap();
        for i in 0..20_000_i64 {
            let address = format!("alex{i:05}@example.test");
            let contact = Contact {
                person: person(&format!("Alex Test {i:05}"), &address),
                last: i,
                counts: Counts {
                    sent: 0,
                    received: 1,
                    ordinary: 1,
                },
            };
            insert
                .execute(params![address, contact.person.name, i])
                .unwrap();
            write_prefixes(&tx, &account, &address, &contact, true).unwrap();
        }
        drop(insert);
        tx.commit().unwrap();

        // Include a one-letter prefix that matches every row, initials, a
        // single address, and a miss. Warm the cached prepared statement.
        let prefixes = ["a", "at", "alex12222@", "no match"];
        for prefix in prefixes {
            assert!(store.correspondent_suggestions(&account, prefix).is_ok());
        }
        let mut timings = Vec::new();
        for (prefix, count) in prefixes.into_iter().zip([6, 6, 1, 0]) {
            for _ in 0..200 {
                let started = Instant::now();
                let result = store.correspondent_suggestions(&account, prefix).unwrap();
                timings.push(started.elapsed());
                assert_eq!(result.len(), count);
            }
        }
        timings.sort_unstable();
        let p95 = timings[timings.len() * 95 / 100];
        eprintln!("20,000 correspondents, 800 queries: p95 = {p95:?}");
        assert!(p95 <= Duration::from_millis(2), "p95 {p95:?} exceeded 2 ms");
    }
}
