//! Styled documents for received HTML and for the markdown composer.
//!
//! The GPUI crate paints [`StyledDocument`]. It does not own the thread list,
//! the keymap, or the command palette. M3 compose should call
//! [`markdown_to_document`] and reuse that same painter.

#![forbid(unsafe_code)]

mod choose;
mod cid;
mod corpus;
mod document;
mod fixtures;
mod history;
mod html;
mod markdown;
mod plain_links;
mod typed_list;
mod webview_html;

pub use choose::{MailHeaders, PreparedMail, RenderMode, choose_view, layout_is_complex, prepare};
pub use cid::visit_cid_image_urls;
pub use corpus::corpus;
pub use document::{
    Align, Block, ImageSource, Inline, ListItem, ListLabelStyle, MAX_DATA_IMAGE_BYTES,
    StyledDocument,
};
pub use fixtures::{Fixture, fixture, fixtures};
pub use history::{
    Fold, FoldedDocument, Section, fold_history, is_forward_subject, is_quoted_from_line,
};
pub use html::{
    document_from_body, html_has_uneditable_draft_formatting, html_to_document, looks_like_html,
    plain_to_document,
};
pub use markdown::markdown_to_document;
pub use plain_links::{plain_link_ranges, reader_link_target};
pub use typed_list::plain_list_runs;
pub use webview_html::{
    WebFrame, address_is_blocked, request_is_blocked, request_is_blocked_with, sanitize_html,
    strip_remote_css_urls, targets_private_network, wrap_for_webview,
};
