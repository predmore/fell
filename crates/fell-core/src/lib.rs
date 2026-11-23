//! UI-free core for Fell: accounts, backends, local store, threading, search, and actions.
//!
//! The GPUI crate and the MCP server depend on this crate. This crate does not
//! depend on them.

#![forbid(unsafe_code)]

pub mod account;
pub mod account_config;
pub mod actions;
pub mod agent;
pub mod backend;
pub mod compose;
pub mod credentials;
#[cfg(unix)]
pub mod diagnostics;
pub mod embed;
pub mod folders;
pub mod imap;
pub mod message;
pub mod notes;
pub mod onboarding;
pub mod outbox;
pub mod parts;
pub mod search;
pub mod send;
mod smtp;
pub mod store;
pub mod sync;
#[doc(hidden)]
pub mod test_support;
pub mod threading;

/// The largest message source sync downloads. A larger message keeps its
/// headers and is marked [`BodyState::TooLarge`].
pub const MAX_MESSAGE_BYTES: usize = 32 * 1024 * 1024;

pub use account::{Account, AccountId, BackendKind};
pub use account_config::{
    AccountConfig, NotificationMode, PasswordSource, SaveSent, SyncFailureKind, classify_sync_error,
};
pub use actions::{ActionKind, FlagChange, LabelChange, LocalAction};
pub use agent::{
    AgentGrant, AgentSummary, RecordedAction, ReviewDraft, ReviewDraftInput, SavedDraft,
};
pub use backend::{
    BridgeBackend, DEMO_MAILBOX_ADDRESS, FetchedBody, FixtureBackend, FolderRemoval, ImapBackend,
    KnownCopies, MailBackend, MailboxDelta, ProtonApiBackend, RemovalKind, Settle, SyncCursor,
    SyncProgress, backend_for, is_demo_account,
};
pub use embed::{
    BodyEmbedder, EMBEDDING_DIMS, EMBEDDING_MODEL, EMBEDDING_VERSION, EmbedderId, Embedding,
    LocalHashV1, embed_body,
};
pub use folders::{FolderRole, FolderRoles, RoleFolder, RoleSource};
pub use imap::{ImapConfig, ImapSecurity, InboxWait};
pub use message::{
    Address, BodyState, Disposition, Message, MessageHeaders, MessageId, MessageParts, MimePart,
    PartRole, RemoteId, TransferEncoding,
};
pub use outbox::{DropReason, DroppedWrite, OutboxEntry, OutboxKind, OutboxState, ReplayReport};
pub use parts::{CachedPart, PartCache, PartEvent, PartFetcher, PartJob, PartRequest};
pub use search::{
    SearchField, SearchHit, SearchLocation, SearchMatch, SearchOptions, SearchPreview,
    SearchResults,
};
pub use send::{
    Draft, PendingSend, PendingSendId, SentMessage, SubmittedFile, UserSendConfirmation,
    message_digest,
};
pub use smtp::SmtpConfig;
pub use store::PendingFlags;
pub use store::{
    CidRepairBatch, CidRepairProgress, FolderPage, FolderSpan, FolderThread, ListFilter, ListKey,
    ListSource, PartKinds, PartStamp, Participant, SearchPage, Store, SyncedDraftPreview,
    ThreadList, ThreadSummary, folder_aliases,
};
pub use sync::{
    InboxArrival, ReplyToBackfill, ReplyToProgress, SyncReport, fetch_message_bodies,
    fetch_message_body, fetch_missing_bodies, sync_once, sync_with_arrivals, sync_with_progress,
};
pub use threading::{ThreadId, header_parent_thread_id, resolve_thread_id};

/// Crate-wide error. IMAP failures use [`Error::Backend`].
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("store: {0}")]
    Store(#[from] rusqlite::Error),
    #[error("search: {0}")]
    Search(String),
    #[error("{0}")]
    Action(String),
    #[error("This draft has no text Fell can edit. The copy is still on the server.")]
    DraftBodyUnavailable,
    #[error("This draft has formatting Fell can't edit yet. The copy is still on the server.")]
    DraftFormattingUnavailable,
    #[error("This message is no longer in Drafts.")]
    DraftNoLongerInFolder,
    #[error("This draft is no longer available on the server.")]
    DraftUnavailable,
    #[error("This draft has too many attachments or its attachments are too large.")]
    DraftAttachmentLimit,
    #[error("This draft exceeds the 32 MiB message limit. The copy is still on the server.")]
    DraftMessageLimit,
    #[error("Undo the newer action first")]
    AgentActivityBlocked(agent::ActivityBlocker),
    #[error("Action already undone")]
    AgentActivityAlreadyUndone { snooze_retired: bool },
    #[error("{0} is not implemented yet")]
    NotImplemented(&'static str),
    #[error("send requires explicit UI confirmation")]
    SendUnconfirmed,
    #[error("{0}")]
    Backend(String),
    #[error("imap: response line from {host} exceeds the {limit} byte limit")]
    ImapLineLimit { host: String, limit: usize },
    #[error(transparent)]
    Keyring(#[from] credentials::KeyringError),
    #[error("{0}")]
    Tls(String),
    /// The SMTP AUTH exchange rejected the configured credentials.
    #[error("{0}")]
    SmtpCredentialsRejected(String),
    /// The server had the whole message before the failure, so it may have
    /// been delivered. Every other send error means nothing went out.
    #[error("{0}")]
    SendUncertain(String),
    #[error("unknown account {0}")]
    UnknownAccount(String),
    #[error("unknown thread")]
    UnknownThread,
    #[error("nothing to undo")]
    NothingToUndo,
    #[error("{0}")]
    Invalid(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Allow for a day of clock skew in mail timestamps.
pub(crate) fn latest_mail_date() -> i64 {
    #[cfg(test)]
    if let Some(latest) = TEST_LATEST_MAIL_DATE.get() {
        return latest;
    }
    outbox::now_unix().saturating_add(86_400)
}

pub(crate) fn plausible_future_mail_date(date: i64, latest: i64) -> bool {
    const MAX_PLAUSIBLE_CLOCK_SKEW_SECONDS: i64 = 30 * 86_400;
    date > latest && date <= latest.saturating_add(MAX_PLAUSIBLE_CLOCK_SKEW_SECONDS)
}

#[cfg(test)]
thread_local! {
    static TEST_LATEST_MAIL_DATE: std::cell::Cell<Option<i64>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub(crate) fn with_latest_mail_date<T>(latest: i64, run: impl FnOnce() -> T) -> T {
    struct Reset(Option<i64>);
    impl Drop for Reset {
        fn drop(&mut self) {
            TEST_LATEST_MAIL_DATE.set(self.0);
        }
    }
    let _reset = Reset(TEST_LATEST_MAIL_DATE.replace(Some(latest)));
    run()
}
