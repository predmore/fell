//! UI-free core for Fell: accounts, backends, local store, threading, and search.
//!
//! The GPUI crate and the MCP stub depend on this crate. This crate does not
//! depend on them.

#![forbid(unsafe_code)]

pub mod account;
pub mod backend;
pub mod message;
pub mod search;
pub mod send;
pub mod store;
pub mod sync;
pub mod threading;

pub use account::{Account, AccountId, BackendKind};
pub use backend::{
    BridgeBackend, FixtureBackend, MailBackend, MailboxDelta, ProtonApiBackend, SyncCursor,
};
pub use message::{Address, Message, MessageHeaders, MessageId, RemoteId};
pub use search::SearchHit;
pub use send::{Draft, PendingSend, PendingSendId, SentMessage, UserSendConfirmation};
pub use store::{Store, ThreadSummary};
pub use sync::{SyncReport, sync_once};
pub use threading::{ThreadId, header_parent_thread_id, resolve_thread_id};

/// Crate-wide error. IMAP/Proton failures will map here once those backends exist.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("store: {0}")]
    Store(#[from] rusqlite::Error),
    #[error("{0} is not implemented yet")]
    NotImplemented(&'static str),
    #[error("send requires explicit UI confirmation")]
    SendUnconfirmed,
    #[error("{0}")]
    Backend(String),
}

pub type Result<T> = std::result::Result<T, Error>;
