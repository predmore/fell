//! Each account's role → folder map, as the last listing found it.

use rusqlite::{Connection, params};

use crate::Result;
use crate::account::AccountId;
use crate::folders::{FolderRole, FolderRoles, RoleFolder, RoleSource};

/// One row per role the server (or `account.toml`) gave a folder. An
/// account with no rows has not been listed yet.
pub(super) const ROLES_TABLE: &str = "
CREATE TABLE IF NOT EXISTS folder_roles (
    account_id TEXT NOT NULL,
    role TEXT NOT NULL,
    folder TEXT NOT NULL,
    raw TEXT NOT NULL,
    source TEXT NOT NULL,
    PRIMARY KEY (account_id, role)
) WITHOUT ROWID;
";

/// Replace the account's roles with `roles`.
pub(super) fn replace(
    conn: &Connection,
    account_id: &AccountId,
    roles: &FolderRoles,
) -> Result<()> {
    let account = account_id.as_str();
    conn.execute(
        "DELETE FROM folder_roles WHERE account_id = ?1",
        params![account],
    )?;
    let mut insert = conn.prepare_cached(
        "INSERT INTO folder_roles (account_id, role, folder, raw, source)
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;
    for folder in roles.iter() {
        insert.execute(params![
            account,
            folder.role.key(),
            folder.name,
            folder.raw,
            folder.source.key()
        ])?;
    }
    Ok(())
}

/// The listed roles, or `None` when no listing was recorded.
pub(super) fn listed(conn: &Connection, account_id: &AccountId) -> Result<Option<FolderRoles>> {
    let mut stmt = conn.prepare_cached(
        "SELECT role, folder, raw, source FROM folder_roles WHERE account_id = ?1",
    )?;
    let rows = stmt.query_map(params![account_id.as_str()], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    let mut folders = Vec::new();
    for row in rows {
        let (role, name, raw, source) = row?;
        // A role a later version added is skipped, not an error.
        let (Some(role), Some(source)) =
            (FolderRole::from_key(&role), RoleSource::from_key(&source))
        else {
            continue;
        };
        folders.push(RoleFolder {
            role,
            name,
            raw,
            source,
        });
    }
    Ok((!folders.is_empty()).then(|| FolderRoles::new(folders)))
}
