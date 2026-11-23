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
mod reply;
mod review;
mod server;
mod session;
mod workspace;
mod writer;

pub use frontmatter::{
    AttachmentRef, DraftFields, DraftIssue, ParsedDraft, email_of, format_address_line,
    mailbox_parts, parse_address_line,
};
pub use hub::{
    ClosedDraftSave, DraftCloseNotice, DraftHub, FAILED_DRAFT_CLOSE, HubEvent,
    LOCAL_SENT_COPY_FAILED, MAYBE_SENT_NOTICE, editor_is_running, file_fallback_edit,
    file_fallback_get, file_fallback_replace_body, request_send_without_editor, rpc_call,
};
pub(crate) use markdown::content_id as draft_content_id;
pub(crate) use markdown::document_to_markdown;
pub use markdown::{
    Block, BlockBody, Inline, StyleKind, StyleSpan, StyledDoc, doc_to_html, html_to_doc,
    line_styles, markdown_to_doc,
};
pub use merge::HunkKind;
pub use mime::{Outgoing, ResolvedFile};
pub use persist::{
    DraftSummary, StoredCompose, ensure_draft_folder, mailbox_db_path, open_owned_draft,
    remember_compose_store, remembered_store_path, stored_compose_account, stored_compose_owner,
};
pub(crate) use persist::{
    confirm_synced_sends, mark_discarded, record_confirmed_send, undo_discard,
};
pub(crate) use reply::format_address;
pub use reply::{
    ForwardSummary, MAX_REFERENCES, ReplyKind, ReplyParts, ReplyStart, build_references,
    configured_from, forward_subject, own_addresses, reply_recipients, reply_subject, start_reply,
    start_reply_in_zone,
};
pub(crate) use server::server_draft_payload;
pub use server::{DraftSaveStatus, SERVER_DRAFT_SAVE_FAILED, SaveReceipt};
pub use session::{
    AddressView, AttachmentView, DraftSession, HunkView, ReplyContext, SendLock, SentCopy, Snapshot,
};
pub(crate) use workspace::inline_image_paths;
pub use workspace::{
    MAX_ATTACHMENT_BYTES, MAX_ATTACHMENT_FILES, control_dir, draft_file_is_symlink, draft_folder,
    draft_root, draft_socket_path, image_file, materialize_in, materialize_workspace, read_account,
    read_owner, require_owner, resolve_inside, write_owner, write_store_link,
};
pub use writer::SaveWaiter;
