//! Read-only generic IMAP. Proton Mail Bridge is one account of this kind.
//!
//! The session lists mailboxes, examines them, and fetches with `BODY.PEEK`
//! so the server is not asked to mark mail read. There is no SMTP, no OAuth,
//! and no send command.

use std::fmt;

use crate::backend::SyncCursor;

/// How the TCP session is protected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImapSecurity {
    /// Cleartext. Local tests, and a Bridge install set to plain IMAP.
    Plain,
    /// Upgrade with STARTTLS after the greeting. Bridge's usual local port.
    StartTls,
    /// Implicit TLS, typically port 993.
    Tls,
}

/// Connection settings for one IMAP account.
///
/// The password is redacted in [`Debug`] output.
#[derive(Clone, PartialEq, Eq)]
pub struct ImapConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub security: ImapSecurity,
    /// Bridge's local certificate is not a public CA. Leave this false for
    /// any host that is not the user's own machine.
    pub accept_invalid_certs: bool,
}

impl fmt::Debug for ImapConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ImapConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("security", &self.security)
            .field("accept_invalid_certs", &self.accept_invalid_certs)
            .finish()
    }
}

impl ImapConfig {
    /// Proton Mail Bridge defaults: local STARTTLS, certificate not public.
    pub fn bridge_local(username: impl Into<String>, password: impl Into<String>) -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 1143,
            username: username.into(),
            password: password.into(),
            security: ImapSecurity::StartTls,
            accept_invalid_certs: true,
        }
    }
}

/// Per-mailbox checkpoint. `uid` is the highest UID already stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailboxCheckpoint {
    pub uidvalidity: u32,
    pub uid: u32,
    pub modseq: Option<u64>,
}

/// Decode a sync cursor written by [`encode_cursor`].
///
/// An empty token and any token this crate did not write are "no checkpoint",
/// so the next sync fetches every mailbox from UID 1.
pub fn decode_cursor(cursor: Option<&SyncCursor>) -> Vec<(String, MailboxCheckpoint)> {
    let Some(cursor) = cursor else {
        return Vec::new();
    };
    let mut lines = cursor.token.lines();
    if lines.next() != Some("fell-imap/1") {
        return Vec::new();
    }
    let mut out = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split('\t');
        let Some(name) = parts.next().and_then(|raw| unescape_mailbox(raw)) else {
            return Vec::new();
        };
        let Some(uidvalidity) = parts.next().and_then(|raw| raw.parse().ok()) else {
            return Vec::new();
        };
        let Some(uid) = parts.next().and_then(|raw| raw.parse().ok()) else {
            return Vec::new();
        };
        let modseq = match parts.next() {
            Some("-") => None,
            Some(raw) => match raw.parse().ok() {
                Some(value) => Some(value),
                None => return Vec::new(),
            },
            None => return Vec::new(),
        };
        if parts.next().is_some() {
            return Vec::new();
        }
        out.push((
            name,
            MailboxCheckpoint {
                uidvalidity,
                uid,
                modseq,
            },
        ));
    }
    out
}

pub fn encode_cursor(account_key: &str, mailboxes: &[(String, MailboxCheckpoint)]) -> SyncCursor {
    let mut token = String::from("fell-imap/1");
    for (name, checkpoint) in mailboxes {
        let modseq = checkpoint
            .modseq
            .map(|value| value.to_string())
            .unwrap_or_else(|| "-".into());
        token.push('\n');
        token.push_str(&escape_mailbox(name));
        token.push('\t');
        token.push_str(&checkpoint.uidvalidity.to_string());
        token.push('\t');
        token.push_str(&checkpoint.uid.to_string());
        token.push('\t');
        token.push_str(&modseq);
    }
    SyncCursor {
        account_key: account_key.to_string(),
        token,
    }
}

fn escape_mailbox(name: &str) -> String {
    name.replace('\\', "\\\\")
        .replace('\t', "\\t")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

fn unescape_mailbox(raw: &str) -> Option<String> {
    let mut out = String::new();
    let mut chars = raw.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next()? {
            '\\' => out.push('\\'),
            't' => out.push('\t'),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            _ => return None,
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_is_redacted_in_debug() {
        let config = ImapConfig::bridge_local("caleb", "secret-bridge");
        let rendered = format!("{config:?}");
        assert!(!rendered.contains("secret-bridge"));
        assert!(rendered.contains("<redacted>"));
        assert!(rendered.contains("1143"));
    }

    #[test]
    fn cursor_round_trips_mailbox_names() {
        let mailboxes = vec![
            (
                "INBOX".into(),
                MailboxCheckpoint {
                    uidvalidity: 9,
                    uid: 40,
                    modseq: Some(12),
                },
            ),
            (
                "Sent\tItems".into(),
                MailboxCheckpoint {
                    uidvalidity: 3,
                    uid: 0,
                    modseq: None,
                },
            ),
        ];
        let cursor = encode_cursor("acct", &mailboxes);
        assert_eq!(cursor.account_key, "acct");
        assert_eq!(decode_cursor(Some(&cursor)), mailboxes);
    }

    #[test]
    fn unknown_cursor_starts_over() {
        let cursor = SyncCursor {
            account_key: "acct".into(),
            token: "not-ours".into(),
        };
        assert!(decode_cursor(Some(&cursor)).is_empty());
        assert!(decode_cursor(None).is_empty());
    }
}
