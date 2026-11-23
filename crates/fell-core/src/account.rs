/// Stable id for a configured mail account (local, not a Proton id).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AccountId(pub String);

impl AccountId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// How this account reaches Proton Mail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    /// Proton Mail Bridge over local IMAP. Bridge is a separate GPL-3.0 process.
    Bridge,
    /// Any other IMAP account. Uses the same SMTP submission path as Bridge.
    Imap,
    /// Direct Proton API. Not implemented yet; same [`crate::MailBackend`] trait.
    ProtonApi,
}

impl BackendKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bridge => "bridge",
            Self::Imap => "imap",
            Self::ProtonApi => "proton_api",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "bridge" => Some(Self::Bridge),
            "imap" => Some(Self::Imap),
            "proton_api" => Some(Self::ProtonApi),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub id: AccountId,
    pub display_name: String,
    pub email: String,
    pub backend_kind: BackendKind,
}

impl Account {
    /// The configured sender used in compose, with a display-safe name.
    pub fn compose_from(&self) -> String {
        crate::compose::format_address(&crate::Address {
            name: Some(crate::notes::display_safe(&self.display_name, false)),
            email: self.email.clone(),
        })
    }
}
