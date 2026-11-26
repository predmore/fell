//! Keep cached copies available for a verified return to their IMAP server.

use rusqlite::{Connection, params};
use sha2::{Digest, Sha256};

use crate::{AccountConfig, AccountId, Error, Result, Store, outbox};

use super::{
    imap_binding_key, immediate_transaction, matches_imap_connection, read_meta, set_meta,
};

pub(super) fn bind(conn: &Connection, config: &AccountConfig) -> Result<()> {
    let login_key = imap_binding_key(config, "login");
    if read_meta(conn, &login_key)?.is_some_and(|login| login != config.username) {
        return Err(Error::Invalid(
            "This local mailbox belongs to a different IMAP login.".into(),
        ));
    }
    let endpoint_key = imap_binding_key(config, "endpoint");
    let previous = read_meta(conn, &endpoint_key)?;
    let endpoint = config.imap_endpoint_key();
    let managed = imap_binding_key(config, "namespaced");
    if previous.as_deref() != Some(&endpoint) || read_meta(conn, &managed)?.is_none() {
        park(conn, config, previous.as_deref())?;
        set_meta(conn, &managed, "1")?;
    }
    set_meta(conn, &login_key, &config.username)?;
    set_meta(conn, &endpoint_key, &endpoint)
}

impl Store {
    pub(crate) fn imap_server_matches(
        &self,
        config: &AccountConfig,
        fingerprint: &str,
    ) -> Result<bool> {
        let server = server_key(config, fingerprint);
        Ok(matches_imap_connection(&self.conn, config)?
            && read_meta(&self.conn, &imap_binding_key(config, "server"))?.as_deref()
                == Some(&server))
    }

    /// Bind the live session before reading a cursor or using an outbox UID.
    pub(crate) fn observe_imap_server(
        &self,
        config: &AccountConfig,
        fingerprint: &str,
    ) -> Result<String> {
        let endpoint = config.imap_endpoint_key();
        let server = server_key(config, fingerprint);
        let server_key = imap_binding_key(config, "server");
        if !matches_imap_connection(&self.conn, config)? {
            return Err(Error::Invalid(
                "The IMAP connection changed. Reopen the account.".into(),
            ));
        }
        if read_meta(&self.conn, &server_key)?.as_deref() == Some(&server) {
            return Ok(server);
        }
        let tx = immediate_transaction(&self.conn)?;
        if !matches_imap_connection(&tx, config)? {
            return Err(Error::Invalid(
                "The IMAP connection changed. Reopen the account.".into(),
            ));
        }
        let old = read_meta(&tx, &server_key)?;
        if old.as_deref() != Some(&server) {
            if old.is_some() {
                park(&tx, config, Some(&endpoint))?;
            }
            restore_copies(&tx, &config.id, &server)?;
            set_meta(&tx, &server_key, &server)?;
        }
        tx.commit()?;
        Ok(server)
    }
}

fn server_key(config: &AccountConfig, fingerprint: &str) -> String {
    format!(
        "{:x}",
        Sha256::digest(format!("{}\n{fingerprint}", config.imap_endpoint_key()))
    )
}

const COPY_COLUMNS: &str = "account_id, mailbox, uidvalidity, uid, message_id, folder,
    modseq, seen, flagged, seen_handled, keywords";
const ORPHAN_COLUMNS: &str = "account_id, message_id, folder";

fn park(conn: &Connection, config: &AccountConfig, endpoint: Option<&str>) -> Result<()> {
    let id = &config.id;
    let key = imap_binding_key(config, "server");
    let server = read_meta(conn, &key)?;
    let account = AccountId(id.clone());
    if server.is_some() {
        let cursor = super::sync_cursor_on(conn, &account)?;
        outbox::endpoint::stamp_mailbox_epochs(conn, &account, cursor.as_ref())?;
    }
    outbox::hold_for_endpoint_change(conn, &account, endpoint, server.as_deref())?;
    let has_unverified_copies = server.is_none()
        && conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM message_copies WHERE account_id = ?1)",
            [id],
            |row| row.get::<_, bool>(0),
        )?;
    if server.is_some() || has_unverified_copies {
        let server = server.as_deref().unwrap_or("unverified");
        for (table, columns) in [
            ("message_copies", COPY_COLUMNS),
            ("copy_orphans", ORPHAN_COLUMNS),
        ] {
            conn.execute_batch(&format!(
                "CREATE TABLE IF NOT EXISTS parked_{table} AS
                 SELECT '' AS server, {columns} FROM {table} WHERE 0;
                 CREATE INDEX IF NOT EXISTS parked_{table}_server ON parked_{table}(account_id, server);"
            ))?;
            conn.execute(
                &format!(
                    "INSERT INTO parked_{table} (server, {columns})
                          SELECT ?2, {columns} FROM {table} WHERE account_id = ?1"
                ),
                params![id, server],
            )?;
        }
    }
    // Only live copies may be used by the reader, sync, or a new outbox row.
    conn.execute(
        "DELETE FROM meta WHERE key = ?1 OR key = 'cursor:' || ?2",
        params![key, id],
    )?;
    for table in [
        "message_copies",
        "copy_orphans",
        "local_flag_intent_copies",
        "server_draft_copies",
        "discarded_server_draft_copies",
        "server_draft_rescans",
        "mailbox_views",
    ] {
        conn.execute(&format!("DELETE FROM {table} WHERE account_id = ?1"), [id])?;
    }
    for table in ["messages", "message_aliases"] {
        conn.execute(
            &format!("UPDATE {table} SET remote_id = '' WHERE account_id = ?1 AND remote_id <> ''"),
            [id],
        )?;
    }
    conn.execute(
        "UPDATE message_bodies SET structure_copy = NULL WHERE account_id = ?1 AND structure_copy IS NOT NULL",
        [id],
    )?;
    Ok(())
}

fn restore_copies(conn: &Connection, id: &str, server: &str) -> Result<()> {
    for (table, columns) in [
        ("message_copies", COPY_COLUMNS),
        ("copy_orphans", ORPHAN_COLUMNS),
    ] {
        if !super::copies::table_exists(conn, &format!("parked_{table}"))? {
            continue;
        }
        // A merge on the intervening server may have turned this ID into an alias.
        conn.execute(
            &format!(
                "UPDATE parked_{table} AS p SET message_id = (
                        SELECT canonical_message_id FROM message_aliases a
                        WHERE a.account_id = p.account_id AND a.message_id = p.message_id
                      ) WHERE account_id = ?1 AND server = ?2 AND EXISTS (
                        SELECT 1 FROM message_aliases a
                        WHERE a.account_id = p.account_id AND a.message_id = p.message_id)"
            ),
            params![id, server],
        )?;
        conn.execute(
            &format!(
                "INSERT OR IGNORE INTO {table} ({columns})
                      SELECT {columns} FROM parked_{table} p WHERE account_id = ?1 AND server = ?2
                        AND EXISTS (SELECT 1 FROM messages m
                          WHERE m.account_id = p.account_id AND m.message_id = p.message_id)"
            ),
            params![id, server],
        )?;
        conn.execute(
            &format!("DELETE FROM parked_{table} WHERE account_id = ?1 AND server = ?2"),
            params![id, server],
        )?;
    }
    Ok(())
}
