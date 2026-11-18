//! Compose drafts: styled markdown, the draft folder, and live co-editing.
//!
//! Nothing in this module transmits mail. The GPUI editor is the only caller
//! that may turn a prepared draft into a [`crate::PendingSend`].

mod digest;
mod frontmatter;
mod hub;
mod markdown;
mod merge;
mod mime;
mod persist;
mod review;
mod session;
mod workspace;
mod writer;

pub use frontmatter::{
    AttachmentRef, DraftFields, DraftIssue, ParsedDraft, email_of, format_address_line,
    parse_address_line,
};
pub use hub::{
    DraftHub, HubEvent, editor_is_running, file_fallback_edit, file_fallback_get,
    file_fallback_replace_body, request_send_without_editor, rpc_call,
};
pub use markdown::{
    Block, BlockBody, Inline, StyleKind, StyleSpan, StyledDoc, doc_to_html, html_to_doc,
    line_styles, markdown_to_doc,
};
pub use merge::HunkKind;
pub use mime::{Outgoing, ResolvedFile};
pub use persist::{
    StoredCompose, ensure_draft_folder, open_owned_draft, remember_compose_store,
    stored_compose_account, stored_compose_owner,
};
pub use session::{
    AddressView, AttachmentView, DraftSession, HunkView, SendLock, SentBcc, Snapshot,
};
pub use workspace::{
    draft_file_is_symlink, draft_folder, draft_root, draft_socket_path, image_file, materialize_in,
    materialize_workspace, read_account, read_owner, require_owner, resolve_inside, write_owner,
    write_store_link,
};
pub use writer::SaveWaiter;
