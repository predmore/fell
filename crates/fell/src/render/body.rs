use fell_core::{
    Account, AccountId, Address, BackendKind, Message, MessageHeaders, MessageId, RemoteId, Store,
    ThreadId,
};
use fell_render::{
    MailHeaders, PreparedMail, RenderMode, StyledDocument, looks_like_html, plain_to_document,
    prepare,
};

/// What the inbox asks the renderer to draw.
///
/// The inbox owns the thread list, the keymap, and the command palette.
/// It forwards toggle and allow-images here. This crate does not register
/// those keys globally.
pub trait MessageBody {
    fn suggested_mode(&self) -> RenderMode;
    fn mode(&self) -> RenderMode;
    fn set_mode(&mut self, mode: RenderMode);
    fn toggle_mode(&mut self);
    fn document(&self) -> &StyledDocument;
    fn source_html(&self) -> &str;
    fn remote_images_allowed(&self) -> bool;
    fn set_remote_images_allowed(&mut self, allowed: bool);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BodyModel {
    mail: PreparedMail,
    mode: RenderMode,
    allow_remote: bool,
}

impl BodyModel {
    pub fn from_prepared(mail: PreparedMail) -> Self {
        let mode = mail.suggested;
        Self {
            mail,
            mode,
            allow_remote: false,
        }
    }

    /// `body_text` from the store on current main.
    ///
    /// That schema has no List-Unsubscribe, Precedence, or ESP columns, so a
    /// row read back from the store stays native. Pass headers to [`prepare`]
    /// when the caller actually has them (the fixture corpus, and sync once
    /// those headers are stored).
    pub fn from_stored(message: &Message) -> Self {
        Self::from_prepared(prepare_stored(message))
    }
}

impl MessageBody for BodyModel {
    fn suggested_mode(&self) -> RenderMode {
        self.mail.suggested
    }

    fn mode(&self) -> RenderMode {
        self.mode
    }

    fn set_mode(&mut self, mode: RenderMode) {
        self.mode = mode;
    }

    fn toggle_mode(&mut self) {
        self.mode = match self.mode {
            RenderMode::Native => RenderMode::Web,
            RenderMode::Web => RenderMode::Native,
        };
    }

    fn document(&self) -> &StyledDocument {
        &self.mail.document
    }

    fn source_html(&self) -> &str {
        &self.mail.source_html
    }

    fn remote_images_allowed(&self) -> bool {
        self.allow_remote
    }

    fn set_remote_images_allowed(&mut self, allowed: bool) {
        self.allow_remote = allowed;
    }
}

pub fn prepare_stored(message: &Message) -> PreparedMail {
    let body = message.body_text.as_deref().unwrap_or("");
    if looks_like_html(body) {
        let mut mail = prepare(body, &MailHeaders::new());
        mail.suggested = RenderMode::Native;
        mail
    } else {
        PreparedMail {
            suggested: RenderMode::Native,
            document: plain_to_document(body),
            source_html: plain_as_html(body),
        }
    }
}

fn plain_as_html(body: &str) -> String {
    let mut html = String::from("<article>");
    for para in body.split("\n\n") {
        let para = para.trim();
        if para.is_empty() {
            continue;
        }
        html.push_str("<p>");
        // A single newline is a line break in the native document. Inside a
        // `<p>`, HTML would otherwise collapse it into a space.
        for (index, line) in para.lines().enumerate() {
            if index > 0 {
                html.push_str("<br>");
            }
            html.push_str(&escape_html(line));
        }
        html.push_str("</p>");
    }
    html.push_str("</article>");
    html
}

fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Write `body_text` through the current store API and read it back.
pub fn round_trip_body(body: &str, subject: &str) -> fell_core::Result<Message> {
    let store = Store::open_in_memory()?;
    let account = Account {
        id: AccountId("local".into()),
        display_name: "Fell".into(),
        email: "reader@local".into(),
        backend_kind: BackendKind::Bridge,
    };
    store.upsert_account(&account)?;
    let message_id = MessageId::new("renderer-preview@local");
    let message = Message {
        account_id: account.id.clone(),
        headers: MessageHeaders {
            message_id: message_id.clone(),
            proton_conversation_id: None,
            in_reply_to: None,
            references: Vec::new(),
            from: Address {
                name: Some("Ada".into()),
                email: "ada@example.com".into(),
            },
            subject: subject.to_string(),
            date_unix: 1_700_000_000,
        },
        remote_id: RemoteId("renderer-preview".into()),
        thread_id: ThreadId::from_message_id(&message_id),
        snippet: snippet(body),
        body_text: Some(body.to_string()),
        unread: true,
        folders: vec!["INBOX".into()],
        labels: Vec::new(),
        arrived_via: Vec::new(),
    };
    let thread_id = store.upsert_message(&message)?;
    let mut messages = store.thread_messages(&account.id, &thread_id)?;
    messages
        .pop()
        .ok_or_else(|| fell_core::Error::Backend("store round trip returned no message".into()))
}

fn snippet(body: &str) -> String {
    let mut compact = String::new();
    for ch in body.chars() {
        if ch.is_whitespace() {
            if !compact.ends_with(' ') && !compact.is_empty() {
                compact.push(' ');
            }
        } else {
            compact.push(ch);
        }
        if compact.len() >= 120 {
            break;
        }
    }
    compact
}

#[cfg(test)]
mod tests {
    use super::*;
    use fell_render::{Block, Inline, fixture};

    #[test]
    fn stored_html_stays_native_without_bulk_headers() {
        let fixture = fixture("web-newsletter").expect("newsletter fixture");
        let message = round_trip_body(&fixture.html, "The week the river slowed").unwrap();
        assert_eq!(message.headers.subject, "The week the river slowed");
        let prepared = prepare_stored(&message);
        assert_eq!(prepared.suggested, RenderMode::Native);
        assert!(prepared.document.plain_text().contains("river"));
        let with_headers = prepare(&fixture.html, &fixture.headers);
        assert_eq!(with_headers.suggested, RenderMode::Web);
    }

    #[test]
    fn stored_plain_text_is_paragraphs() {
        let message = round_trip_body("Hello\n\nNext line", "Plain").unwrap();
        let prepared = prepare_stored(&message);
        assert_eq!(prepared.suggested, RenderMode::Native);
        assert_eq!(prepared.document.blocks.len(), 2);
    }

    #[test]
    fn stored_plain_text_keeps_a_single_newline() {
        let message = round_trip_body("Hello\nWorld", "Plain").unwrap();
        let prepared = prepare_stored(&message);
        let Block::Paragraph { inlines } = &prepared.document.blocks[0] else {
            panic!("{:?}", prepared.document.blocks);
        };
        assert!(
            inlines
                .iter()
                .any(|inline| matches!(inline, Inline::LineBreak))
        );
        assert!(
            prepared.source_html.contains("<p>Hello<br>World</p>"),
            "{}",
            prepared.source_html
        );
    }
}
