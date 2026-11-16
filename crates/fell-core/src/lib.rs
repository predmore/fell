//! UI-free core for Fell: accounts, backends, local store, threading, search, and actions.
//!
//! The GPUI crate and the MCP server depend on this crate. This crate does not
//! depend on them.

#![forbid(unsafe_code)]

pub mod account;
pub mod actions;
pub mod agent;
pub mod backend;
pub mod imap;
pub mod message;
pub mod search;
pub mod send;
pub mod store;
pub mod sync;
pub mod threading;

pub use account::{Account, AccountId, BackendKind};
pub use actions::{ActionKind, LabelChange, LocalAction};
pub use agent::{AgentGrant, RecordedAction, ReviewDraft, ReviewDraftInput, SavedDraft};
pub use backend::{
    BridgeBackend, FixtureBackend, ImapBackend, MailBackend, MailboxDelta, ProtonApiBackend,
    SyncCursor,
};
pub use imap::{ImapConfig, ImapSecurity};
pub use message::{Address, Message, MessageHeaders, MessageId, RemoteId};
pub use search::{SearchHit, SearchResults};
pub use send::{Draft, PendingSend, PendingSendId, SentMessage, UserSendConfirmation};
pub use store::{SearchPage, Store, ThreadSummary};
pub use sync::{SyncReport, sync_once};
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
    #[error("{0} is not implemented yet")]
    NotImplemented(&'static str),
    #[error("send requires explicit UI confirmation")]
    SendUnconfirmed,
    #[error("{0}")]
    Backend(String),
    #[error("unknown account {0}")]
    UnknownAccount(String),
    #[error("unknown thread {0}")]
    UnknownThread(String),
    #[error("nothing to undo")]
    NothingToUndo,
    #[error("{0}")]
    Invalid(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
