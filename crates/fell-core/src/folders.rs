//! What each folder is for: Inbox, Sent, Drafts, Trash, Junk, Archive, All
//! Mail, Flagged.
//!
//! Sync reads roles from the server's LIST reply (RFC 6154 SPECIAL-USE
//! attributes). A server that marks nothing gets the name table in
//! [`FolderRole::fallback_names`], and `account.toml` can name a folder for
//! any role. The store keeps the result per account ([`FolderRoles`]).
//! Actions, `gi`/`gs`/`gd`, and the folder picker ask for a role, never for a
//! spelling. The UI shows [`FolderRole::label`] while IMAP keeps the server's
//! raw mailbox name.

use std::fmt;

/// A folder's job in the mailbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum FolderRole {
    Inbox,
    Sent,
    Drafts,
    Trash,
    Junk,
    Archive,
    All,
    Flagged,
}

impl FolderRole {
    pub const ALL: [FolderRole; 8] = [
        FolderRole::Inbox,
        FolderRole::Sent,
        FolderRole::Drafts,
        FolderRole::Trash,
        FolderRole::Junk,
        FolderRole::Archive,
        FolderRole::All,
        FolderRole::Flagged,
    ];

    /// The name the UI shows for a folder with this role.
    pub fn label(self) -> &'static str {
        match self {
            Self::Inbox => "Inbox",
            Self::Sent => "Sent",
            Self::Drafts => "Drafts",
            Self::Trash => "Trash",
            Self::Junk => "Spam",
            Self::Archive => "Archive",
            Self::All => "All Mail",
            Self::Flagged => "Starred",
        }
    }

    /// Stable id for the store and `account.toml` keys.
    pub fn key(self) -> &'static str {
        match self {
            Self::Inbox => "inbox",
            Self::Sent => "sent",
            Self::Drafts => "drafts",
            Self::Trash => "trash",
            Self::Junk => "junk",
            Self::Archive => "archive",
            Self::All => "all_mail",
            Self::Flagged => "flagged",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|role| role.key() == key)
    }

    /// The `account.toml` key that names this role's folder. Inbox has none:
    /// IMAP always calls it `INBOX`.
    pub fn config_key(self) -> Option<&'static str> {
        match self {
            Self::Inbox => None,
            Self::Sent => Some("sent_folder"),
            Self::Drafts => Some("drafts_folder"),
            Self::Trash => Some("trash_folder"),
            Self::Junk => Some("junk_folder"),
            Self::Archive => Some("archive_folder"),
            Self::All => Some("all_mail_folder"),
            Self::Flagged => Some("flagged_folder"),
        }
    }

    /// The RFC 6154 attribute (or the older XLIST spelling) for this role.
    pub fn from_attribute(attribute: &str) -> Option<Self> {
        let name = attribute.strip_prefix('\\')?;
        let role = match name.to_ascii_lowercase().as_str() {
            "sent" => Self::Sent,
            "drafts" => Self::Drafts,
            "trash" => Self::Trash,
            "junk" | "spam" => Self::Junk,
            "archive" => Self::Archive,
            "all" | "allmail" => Self::All,
            "flagged" | "starred" => Self::Flagged,
            "inbox" => Self::Inbox,
            _ => return None,
        };
        Some(role)
    }

    /// Names that mean this role on a server that marks nothing, best first.
    /// The first is also the folder the store uses before any listing.
    ///
    /// This is the only place Fell spells a folder's name.
    pub fn fallback_names(self) -> &'static [&'static str] {
        match self {
            Self::Inbox => &["INBOX"],
            Self::Sent => &["Sent", "Sent Items", "Sent Messages", "Sent Mail"],
            Self::Drafts => &["Drafts", "Draft"],
            Self::Trash => &[
                "Trash",
                "Deleted Items",
                "Deleted Messages",
                "Deleted",
                "Bin",
            ],
            Self::Junk => &["Spam", "Junk", "Junk E-mail", "Junk Email", "Bulk Mail"],
            Self::Archive => &["Archive", "Archives", "Archived"],
            Self::All => &["All Mail"],
            Self::Flagged => &["Starred", "Flagged"],
        }
    }

    /// The folder this role means when nothing better is known.
    pub fn default_name(self) -> &'static str {
        self.fallback_names()[0]
    }
}

impl fmt::Display for FolderRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// How a role's folder was chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoleSource {
    /// The server marked it (SPECIAL-USE).
    Server,
    /// The fallback name table.
    Name,
    /// `account.toml`.
    Config,
}

impl RoleSource {
    pub fn key(self) -> &'static str {
        match self {
            Self::Server => "server",
            Self::Name => "name",
            Self::Config => "config",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        match key {
            "server" => Some(Self::Server),
            "name" => Some(Self::Name),
            "config" => Some(Self::Config),
            _ => None,
        }
    }
}

/// One role and the folder that has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleFolder {
    pub role: FolderRole,
    /// The folder as messages store it (decoded from modified UTF-7).
    pub name: String,
    /// The mailbox name to send in IMAP commands.
    pub raw: String,
    pub source: RoleSource,
}

/// A mailbox from the server's LIST reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedMailbox {
    pub raw: String,
    pub name: String,
    /// Hierarchy delimiter, if the server has one.
    pub delimiter: Option<char>,
    /// LIST attributes, such as `\HasNoChildren` or `\Sent`.
    pub attributes: Vec<String>,
}

/// Each role's folder for one account.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FolderRoles {
    folders: Vec<RoleFolder>,
}

impl FolderRoles {
    pub fn new(mut folders: Vec<RoleFolder>) -> Self {
        folders.sort_by_key(|folder| folder.role);
        folders.dedup_by_key(|folder| folder.role);
        Self { folders }
    }

    /// Roles from a LIST reply. `overrides` (from `account.toml`) win. Then
    /// SPECIAL-USE attributes; the name table is used only when the server
    /// marked no mailbox for any role but the inbox. INBOX is always the
    /// inbox.
    pub fn detect(listed: &[ListedMailbox], overrides: &[(FolderRole, String)]) -> Self {
        let mut found: Vec<RoleFolder> = Vec::new();
        let put = |found: &mut Vec<RoleFolder>, folder: RoleFolder| {
            if !found.iter().any(|have| have.role == folder.role) {
                found.push(folder);
            }
        };
        for (role, name) in overrides {
            let name = name.trim();
            if name.is_empty() {
                continue;
            }
            let listed_as = listed
                .iter()
                .find(|mailbox| mailbox.name.eq_ignore_ascii_case(name))
                .or_else(|| listed.iter().find(|mailbox| mailbox.raw == name));
            let (name, raw) = match listed_as {
                Some(mailbox) => (mailbox.name.clone(), mailbox.raw.clone()),
                None => (name.to_string(), name.to_string()),
            };
            put(
                &mut found,
                RoleFolder {
                    role: *role,
                    name,
                    raw,
                    source: RoleSource::Config,
                },
            );
        }
        if let Some(inbox) = listed
            .iter()
            .find(|mailbox| mailbox.name.eq_ignore_ascii_case("INBOX"))
        {
            put(
                &mut found,
                role_folder(FolderRole::Inbox, inbox, RoleSource::Server),
            );
        }
        let mut marked = false;
        for mailbox in listed {
            for attribute in &mailbox.attributes {
                if let Some(role) = FolderRole::from_attribute(attribute) {
                    // XLIST's `\Inbox` can name an inbox not called INBOX,
                    // but it is not RFC 6154: a server that marks only its
                    // inbox has said nothing about Sent or Trash.
                    marked |= role != FolderRole::Inbox;
                    put(&mut found, role_folder(role, mailbox, RoleSource::Server));
                }
            }
        }
        if !marked {
            for role in FolderRole::ALL {
                if let Some(mailbox) = by_name(listed, role) {
                    put(&mut found, role_folder(role, mailbox, RoleSource::Name));
                }
            }
        }
        Self::new(found)
    }

    /// Roles for an account no server has listed yet (the demo mailbox, or
    /// mail stored before roles were recorded): the name table over the
    /// folder names already stored. The delimiter is not stored, so a name
    /// under INBOX may use either `/` or `.` (`INBOX.Sent`).
    pub fn from_names(names: &[String]) -> Self {
        let listed: Vec<ListedMailbox> = names
            .iter()
            .map(|name| ListedMailbox {
                raw: name.clone(),
                name: name.clone(),
                delimiter: Some(stored_delimiter(name)),
                attributes: Vec::new(),
            })
            .collect();
        Self::detect(&listed, &[])
    }

    pub fn is_empty(&self) -> bool {
        self.folders.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &RoleFolder> {
        self.folders.iter()
    }

    pub fn get(&self, role: FolderRole) -> Option<&RoleFolder> {
        self.folders.iter().find(|folder| folder.role == role)
    }

    /// The folder for `role`, or [`FolderRole::default_name`] when the
    /// account has none.
    pub fn name(&self, role: FolderRole) -> &str {
        self.get(role)
            .map(|folder| folder.name.as_str())
            .unwrap_or_else(|| role.default_name())
    }

    /// Where archive puts mail: the Archive folder, else All Mail (Gmail,
    /// where archiving leaves mail only there), else the default name.
    pub fn archive_name(&self) -> &str {
        self.get(FolderRole::Archive)
            .or_else(|| self.get(FolderRole::All))
            .map(|folder| folder.name.as_str())
            .unwrap_or_else(|| FolderRole::Archive.default_name())
    }

    /// The role `folder` has, matching names without regard to ASCII case.
    pub fn role_of(&self, folder: &str) -> Option<FolderRole> {
        if folder.eq_ignore_ascii_case("INBOX") {
            return Some(FolderRole::Inbox);
        }
        if let Some(found) = self
            .folders
            .iter()
            .find(|have| have.name.eq_ignore_ascii_case(folder))
        {
            return Some(found.role);
        }
        // A role with no folder yet answers to its default name, so the
        // names [`Self::name`] hands out read back as that role.
        FolderRole::ALL.into_iter().find(|role| {
            self.get(*role).is_none() && role.default_name().eq_ignore_ascii_case(folder)
        })
    }

    pub fn is(&self, role: FolderRole, folder: &str) -> bool {
        self.role_of(folder) == Some(role)
    }

    /// What the UI calls `folder`: its role's label, else its own name.
    pub fn label(&self, folder: &str) -> String {
        match self.role_of(folder) {
            Some(role) => role.label().to_string(),
            None => folder.to_string(),
        }
    }

    /// The folder's own name, when its label alone reads like another
    /// folder in `names`: on Gmail `[Gmail]/Sent Mail` shows as "Sent", and
    /// so does a label called `Sent`. Only the folder shown under a role's
    /// label gets one; a folder shown under its own name never does, and
    /// neither does any folder when no label is shared.
    pub fn label_detail<'a>(&self, folder: &'a str, names: &[String]) -> Option<&'a str> {
        let label = self.label(folder);
        if label.eq_ignore_ascii_case(folder) {
            return None;
        }
        names
            .iter()
            .map(String::as_str)
            .chain(self.folders.iter().map(|have| have.name.as_str()))
            .filter(|other| !other.eq_ignore_ascii_case(folder))
            .any(|other| self.label(other).eq_ignore_ascii_case(&label))
            .then_some(folder)
    }

    /// [`Self::label`] on one line of text, with the folder's own name in
    /// brackets when [`Self::label_detail`] has one: `Sent ([Gmail]/Sent
    /// Mail)`.
    pub fn label_among(&self, folder: &str, names: &[String]) -> String {
        let label = self.label(folder);
        match self.label_detail(folder, names) {
            Some(name) => format!("{label} ({name})"),
            None => label,
        }
    }

    /// Folders for the picker: Inbox, Sent, Drafts, Archive, Spam, and Trash
    /// always, then All Mail and Starred when the account has them, then the
    /// rest of `stored` in order. Each folder appears once.
    pub fn picker(&self, stored: &[String]) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let push = |out: &mut Vec<String>, name: &str| {
            if !out.iter().any(|have| have.eq_ignore_ascii_case(name)) {
                out.push(name.to_string());
            }
        };
        push(&mut out, self.name(FolderRole::Inbox));
        push(&mut out, self.name(FolderRole::Sent));
        push(&mut out, self.name(FolderRole::Drafts));
        push(&mut out, self.archive_name());
        push(&mut out, self.name(FolderRole::Junk));
        push(&mut out, self.name(FolderRole::Trash));
        for role in [FolderRole::All, FolderRole::Flagged] {
            if let Some(folder) = self.get(role) {
                push(&mut out, &folder.name);
            }
        }
        for name in stored {
            push(&mut out, name);
        }
        out
    }
}

fn role_folder(role: FolderRole, mailbox: &ListedMailbox, source: RoleSource) -> RoleFolder {
    RoleFolder {
        role,
        name: mailbox.name.clone(),
        raw: mailbox.raw.clone(),
        source,
    }
}

/// The hierarchy delimiter a stored name seems to use: `.` for a name
/// directly under INBOX written `INBOX.`, else `/`.
fn stored_delimiter(name: &str) -> char {
    match name.get(..6) {
        Some(head) if head.eq_ignore_ascii_case("INBOX.") => '.',
        _ => '/',
    }
}

/// The best mailbox for `role` by name: a top-level match first, then one
/// directly under INBOX (`INBOX.Sent` on servers that nest everything there).
fn by_name(listed: &[ListedMailbox], role: FolderRole) -> Option<&ListedMailbox> {
    let names = role.fallback_names();
    for wanted in names {
        if let Some(found) = listed
            .iter()
            .find(|mailbox| mailbox.name.eq_ignore_ascii_case(wanted))
        {
            return Some(found);
        }
    }
    if role == FolderRole::Inbox {
        return None;
    }
    for wanted in names {
        if let Some(found) = listed.iter().find(|mailbox| {
            mailbox.delimiter.is_some_and(|delimiter| {
                mailbox
                    .name
                    .split_once(delimiter)
                    .is_some_and(|(parent, leaf)| {
                        parent.eq_ignore_ascii_case("INBOX") && leaf.eq_ignore_ascii_case(wanted)
                    })
            })
        }) {
            return Some(found);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mailbox(name: &str, attributes: &[&str]) -> ListedMailbox {
        ListedMailbox {
            raw: name.into(),
            name: name.into(),
            delimiter: Some('/'),
            attributes: attributes.iter().map(|flag| (*flag).to_string()).collect(),
        }
    }

    #[test]
    fn special_use_attributes_name_each_role() {
        let roles = FolderRoles::detect(
            &[
                mailbox("INBOX", &["\\HasNoChildren"]),
                mailbox("[Gmail]", &["\\Noselect", "\\HasChildren"]),
                mailbox("[Gmail]/All Mail", &["\\All", "\\HasNoChildren"]),
                mailbox("[Gmail]/Drafts", &["\\Drafts"]),
                mailbox("[Gmail]/Sent Mail", &["\\Sent"]),
                mailbox("[Gmail]/Spam", &["\\Junk"]),
                mailbox("[Gmail]/Starred", &["\\Flagged"]),
                mailbox("[Gmail]/Bin", &["\\Trash"]),
                // A label named like a role is not that role once the
                // server marks its folders.
                mailbox("Sent", &[]),
            ],
            &[],
        );
        assert_eq!(roles.name(FolderRole::Sent), "[Gmail]/Sent Mail");
        assert_eq!(roles.name(FolderRole::Trash), "[Gmail]/Bin");
        assert_eq!(roles.name(FolderRole::Junk), "[Gmail]/Spam");
        assert_eq!(roles.name(FolderRole::Flagged), "[Gmail]/Starred");
        assert_eq!(roles.get(FolderRole::Archive), None);
        assert_eq!(roles.archive_name(), "[Gmail]/All Mail");
        assert_eq!(roles.role_of("sent"), None);
        assert_eq!(roles.label("[Gmail]/Sent Mail"), "Sent");
        assert_eq!(roles.label("Sent"), "Sent");
        assert!(
            roles
                .iter()
                .all(|folder| folder.source == RoleSource::Server)
        );
    }

    #[test]
    fn xlist_spellings_count_as_special_use() {
        let roles = FolderRoles::detect(
            &[
                mailbox("Posteingang", &["\\Inbox"]),
                mailbox("Alle", &["\\AllMail"]),
                mailbox("Werbung", &["\\Spam"]),
                mailbox("Markiert", &["\\Starred"]),
            ],
            &[],
        );
        assert_eq!(roles.name(FolderRole::Inbox), "Posteingang");
        assert_eq!(roles.name(FolderRole::All), "Alle");
        assert_eq!(roles.name(FolderRole::Junk), "Werbung");
        assert_eq!(roles.name(FolderRole::Flagged), "Markiert");
    }

    #[test]
    fn names_are_used_only_when_the_server_marks_nothing() {
        let unmarked = FolderRoles::detect(
            &[
                mailbox("INBOX", &[]),
                mailbox("Sent Messages", &[]),
                mailbox("Deleted Messages", &[]),
                mailbox("Junk", &[]),
                mailbox("Work", &[]),
            ],
            &[],
        );
        assert_eq!(unmarked.name(FolderRole::Sent), "Sent Messages");
        assert_eq!(unmarked.name(FolderRole::Trash), "Deleted Messages");
        assert_eq!(unmarked.name(FolderRole::Junk), "Junk");
        assert_eq!(
            unmarked.get(FolderRole::Sent).unwrap().source,
            RoleSource::Name
        );
        // Nothing listed for drafts: the default name stands in.
        assert_eq!(unmarked.get(FolderRole::Drafts), None);
        assert_eq!(unmarked.name(FolderRole::Drafts), "Drafts");
        assert_eq!(unmarked.role_of("Work"), None);

        let marked = FolderRoles::detect(
            &[
                mailbox("INBOX", &[]),
                mailbox("Sent Items", &["\\Sent"]),
                mailbox("Trash", &[]),
            ],
            &[],
        );
        assert_eq!(marked.name(FolderRole::Sent), "Sent Items");
        assert_eq!(marked.get(FolderRole::Trash), None);
    }

    #[test]
    fn an_xlist_inbox_alone_still_leaves_the_name_table_on() {
        let roles = FolderRoles::detect(
            &[
                mailbox("INBOX", &["\\Inbox", "\\HasNoChildren"]),
                mailbox("Sent Items", &["\\HasNoChildren"]),
                mailbox("Deleted Messages", &["\\HasNoChildren"]),
            ],
            &[],
        );
        assert_eq!(roles.name(FolderRole::Inbox), "INBOX");
        assert_eq!(roles.name(FolderRole::Sent), "Sent Items");
        assert_eq!(roles.name(FolderRole::Trash), "Deleted Messages");
        assert_eq!(
            roles.get(FolderRole::Sent).unwrap().source,
            RoleSource::Name
        );

        // `\Inbox` still names an inbox that is not called INBOX.
        let roles = FolderRoles::detect(
            &[mailbox("Posteingang", &["\\Inbox"]), mailbox("Sent", &[])],
            &[],
        );
        assert_eq!(roles.name(FolderRole::Inbox), "Posteingang");
        assert_eq!(roles.name(FolderRole::Sent), "Sent");
        assert_eq!(
            roles.get(FolderRole::Sent).unwrap().source,
            RoleSource::Name
        );
    }

    #[test]
    fn a_label_named_like_a_role_is_told_apart_from_the_role_folder() {
        let roles = FolderRoles::detect(
            &[
                mailbox("INBOX", &[]),
                mailbox("[Gmail]/Sent Mail", &["\\Sent"]),
                mailbox("[Gmail]/Spam", &["\\Junk"]),
                mailbox("[Gmail]/Trash", &["\\Trash"]),
                mailbox("Sent", &[]),
                mailbox("Receipts", &[]),
            ],
            &[],
        );
        let names: Vec<String> = [
            "INBOX",
            "[Gmail]/Sent Mail",
            "[Gmail]/Spam",
            "Sent",
            "Receipts",
        ]
        .map(String::from)
        .to_vec();
        // Both read "Sent": the server's folder says which it is, the
        // label stays plain.
        assert_eq!(roles.label("[Gmail]/Sent Mail"), "Sent");
        assert_eq!(roles.label("Sent"), "Sent");
        assert_eq!(
            roles.label_detail("[Gmail]/Sent Mail", &names),
            Some("[Gmail]/Sent Mail")
        );
        assert_eq!(roles.label_detail("Sent", &names), None);
        assert_eq!(
            roles.label_among("[Gmail]/Sent Mail", &names),
            "Sent ([Gmail]/Sent Mail)"
        );
        assert_eq!(roles.label_among("Sent", &names), "Sent");
        // No collision, no detail.
        assert_eq!(roles.label_detail("[Gmail]/Spam", &names), None);
        assert_eq!(roles.label_detail("[Gmail]/Trash", &names), None);
        assert_eq!(roles.label_detail("Receipts", &names), None);
        assert_eq!(roles.label_detail("INBOX", &names), None);

        // A case-only difference still reads the same.
        let names = vec!["[Gmail]/Spam".to_string(), "spam".to_string()];
        assert_eq!(
            roles.label_detail("[Gmail]/Spam", &names),
            Some("[Gmail]/Spam")
        );
        assert_eq!(roles.label_detail("spam", &names), None);
    }

    #[test]
    fn stored_names_under_inbox_match_with_either_delimiter() {
        let dotted = FolderRoles::from_names(&[
            "INBOX".into(),
            "INBOX.Sent".into(),
            "INBOX.Trash".into(),
            "INBOX.Work".into(),
        ]);
        assert_eq!(dotted.name(FolderRole::Sent), "INBOX.Sent");
        assert_eq!(dotted.name(FolderRole::Trash), "INBOX.Trash");
        assert_eq!(dotted.label("INBOX.Sent"), "Sent");
        assert_eq!(dotted.role_of("INBOX.Work"), None);

        let slashed = FolderRoles::from_names(&["INBOX".into(), "INBOX/Drafts".into()]);
        assert_eq!(slashed.name(FolderRole::Drafts), "INBOX/Drafts");
    }

    #[test]
    fn folders_nested_under_inbox_match_by_their_leaf() {
        let listed = vec![
            ListedMailbox {
                raw: "INBOX".into(),
                name: "INBOX".into(),
                delimiter: Some('.'),
                attributes: Vec::new(),
            },
            ListedMailbox {
                raw: "INBOX.Sent".into(),
                name: "INBOX.Sent".into(),
                delimiter: Some('.'),
                attributes: Vec::new(),
            },
            ListedMailbox {
                raw: "INBOX.Trash".into(),
                name: "INBOX.Trash".into(),
                delimiter: Some('.'),
                attributes: Vec::new(),
            },
        ];
        let roles = FolderRoles::detect(&listed, &[]);
        assert_eq!(roles.name(FolderRole::Sent), "INBOX.Sent");
        assert_eq!(roles.name(FolderRole::Trash), "INBOX.Trash");
        assert_eq!(roles.name(FolderRole::Inbox), "INBOX");
    }

    #[test]
    fn account_overrides_win_and_keep_the_server_spelling() {
        let mut gesendet = mailbox("Gesendet", &[]);
        gesendet.raw = "Gesendet".into();
        let mut entwuerfe = mailbox("Entwürfe", &[]);
        entwuerfe.raw = "Entw&APw-rfe".into();
        let roles = FolderRoles::detect(
            &[
                mailbox("INBOX", &[]),
                gesendet,
                entwuerfe,
                mailbox("Sent", &["\\Sent"]),
            ],
            &[
                (FolderRole::Sent, "gesendet".into()),
                (FolderRole::Drafts, "Entwürfe".into()),
                (FolderRole::Archive, "Ablage".into()),
            ],
        );
        let sent = roles.get(FolderRole::Sent).unwrap();
        assert_eq!(sent.name, "Gesendet");
        assert_eq!(sent.source, RoleSource::Config);
        assert_eq!(roles.get(FolderRole::Drafts).unwrap().raw, "Entw&APw-rfe");
        // A folder the server did not list is still the one asked for.
        assert_eq!(roles.archive_name(), "Ablage");
    }

    #[test]
    fn an_unlisted_account_reads_roles_from_stored_names() {
        let roles = FolderRoles::from_names(&["INBOX".into(), "Junk".into(), "Archived".into()]);
        assert_eq!(roles.name(FolderRole::Junk), "Junk");
        assert_eq!(roles.archive_name(), "Archived");
        assert_eq!(roles.label("junk"), "Spam");
        // Roles with no stored folder answer to their default name.
        assert_eq!(roles.name(FolderRole::Sent), "Sent");
        assert!(roles.is(FolderRole::Sent, "sent"));
        assert!(!roles.is(FolderRole::Archive, "Archive"));
    }

    #[test]
    fn the_picker_lists_role_folders_first_and_each_once() {
        let roles = FolderRoles::detect(
            &[
                mailbox("INBOX", &[]),
                mailbox("[Gmail]/All Mail", &["\\All"]),
                mailbox("[Gmail]/Sent Mail", &["\\Sent"]),
                mailbox("[Gmail]/Trash", &["\\Trash"]),
                mailbox("[Gmail]/Spam", &["\\Junk"]),
                mailbox("[Gmail]/Drafts", &["\\Drafts"]),
            ],
            &[],
        );
        let picker = roles.picker(&[
            "inbox".into(),
            "Work".into(),
            "[Gmail]/Sent Mail".into(),
            "[Gmail]/All Mail".into(),
        ]);
        assert_eq!(
            picker,
            vec![
                "INBOX",
                "[Gmail]/Sent Mail",
                "[Gmail]/Drafts",
                "[Gmail]/All Mail",
                "[Gmail]/Spam",
                "[Gmail]/Trash",
                "Work",
            ]
        );
    }

    #[test]
    fn role_keys_round_trip() {
        for role in FolderRole::ALL {
            assert_eq!(FolderRole::from_key(role.key()), Some(role));
        }
        for source in [RoleSource::Server, RoleSource::Name, RoleSource::Config] {
            assert_eq!(RoleSource::from_key(source.key()), Some(source));
        }
    }
}
