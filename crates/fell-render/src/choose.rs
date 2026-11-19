/// Native unless bulk-mail signals and layout complexity are both present.
///
/// Personal and conversational mail stays native even when the HTML is a
/// forwarded newsletter. A bulk message with ordinary paragraphs stays native
/// too: the web view is for mail whose layout we would visibly break.
use crate::document::StyledDocument;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderMode {
    Native,
    Web,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MailHeaders {
    fields: Vec<(String, String)>,
}

impl MailHeaders {
    pub fn new() -> Self {
        Self { fields: Vec::new() }
    }

    pub fn add(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.fields.push((name.into(), value.into()));
    }

    /// `Name: value` lines. Blank lines and `#` comments are ignored.
    pub fn from_lines(text: &str) -> Self {
        let mut headers = Self::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((name, value)) = line.split_once(':') {
                headers.add(name.trim(), value.trim());
            }
        }
        headers
    }

    /// Every field, in order.
    pub fn fields(&self) -> impl Iterator<Item = (&str, &str)> {
        self.fields
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.fields
            .iter()
            .find_map(|(field, value)| field.eq_ignore_ascii_case(name).then_some(value.as_str()))
    }

    pub fn is_bulk(&self) -> bool {
        self.fields
            .iter()
            .any(|(name, value)| bulk_field(name, value))
    }
}

pub fn choose_view(headers: &MailHeaders, html: &str) -> RenderMode {
    if headers.is_bulk() && layout_is_complex(html) {
        RenderMode::Web
    } else {
        RenderMode::Native
    }
}

/// A message the UI can paint. `source_html` is what the web view loads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedMail {
    pub suggested: RenderMode,
    pub document: StyledDocument,
    pub source_html: String,
}

pub fn prepare(html: &str, headers: &MailHeaders) -> PreparedMail {
    PreparedMail {
        suggested: choose_view(headers, html),
        document: crate::html_to_document(html),
        source_html: html.to_string(),
    }
}

fn bulk_field(name: &str, value: &str) -> bool {
    if value.trim().is_empty() {
        return false;
    }
    if name.eq_ignore_ascii_case("list-unsubscribe") {
        return true;
    }
    if name.eq_ignore_ascii_case("precedence") {
        return value
            .split(|c: char| c == ',' || c.is_whitespace())
            .any(|token| token.eq_ignore_ascii_case("bulk") || token.eq_ignore_ascii_case("list"));
    }
    if name.eq_ignore_ascii_case("x-mailer") {
        return mailer_is_esp(value);
    }
    esp_header(name)
}

fn esp_header(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    const PREFIXES: &[&str] = &[
        "x-mailchimp-",
        "x-mc-",
        "x-mailgun-",
        "x-ses-",
        "x-sendgrid-",
        "x-klaviyo-",
        "x-mailjet-",
        "x-mandrill-",
        "x-hs-",
        "x-hubspot-",
        "x-cmail-",
        "x-campaign-id",
        "x-sg-eid",
        "x-sg-id",
        "x-pm-message-id",
        "x-pm-tag",
        "x-pm-rcpt",
        "feedback-id",
    ];
    PREFIXES
        .iter()
        .any(|prefix| name == *prefix || name.starts_with(prefix))
}

fn mailer_is_esp(value: &str) -> bool {
    const NEEDLES: &[&str] = &[
        "mailchimp",
        "sendgrid",
        "amazon ses",
        "amazonses",
        "constant contact",
        "campaign monitor",
        "mailgun",
        "postmark",
        "klaviyo",
        "convertkit",
        "mailjet",
        "hubspot",
        "marketo",
        "customer.io",
        "braze",
    ];
    let value = value.to_ascii_lowercase();
    NEEDLES.iter().any(|needle| value.contains(needle))
}

/// Table layout or CSS we would not paint faithfully.
pub fn layout_is_complex(html: &str) -> bool {
    let lower = html.to_ascii_lowercase();
    let presentation = lower.contains("role=\"presentation\"")
        || lower.contains("role='presentation'")
        || lower.contains("role=\"none\"")
        || lower.contains("role='none'");
    let nested_tables = lower.matches("<table").count() >= 2;
    let layout_tables = presentation || nested_tables;
    let style_bytes = style_payload_len(&lower);
    let inline_styles = lower.matches("style=").count();
    let heavy = heavy_css(&lower);
    (layout_tables && (heavy || style_bytes > 800 || inline_styles > 12))
        || (heavy && style_bytes > 1500)
}

fn style_payload_len(lower: &str) -> usize {
    let mut total = 0;
    let mut rest = lower;
    while let Some(start) = rest.find("<style") {
        rest = &rest[start..];
        let Some(open) = rest.find('>') else { break };
        rest = &rest[open + 1..];
        let Some(end) = rest.find("</style") else {
            break;
        };
        total += end;
        rest = &rest[end..];
    }
    total
}

fn heavy_css(lower: &str) -> bool {
    lower.contains("float:")
        || lower.contains("position:absolute")
        || lower.contains("position: absolute")
        || lower.contains("position:fixed")
        || lower.contains("position: fixed")
        || lower.contains("display:flex")
        || lower.contains("display: flex")
        || lower.contains("display:grid")
        || lower.contains("display: grid")
        || lower.contains("@media")
        || lower.contains("background-image")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn personal_mail_stays_native_when_the_html_is_complex() {
        let html = include_str!("../fixtures/web-newsletter.html");
        assert!(layout_is_complex(html));
        let headers = MailHeaders::from_lines("From: Ada <ada@example.com>\nSubject: Fwd: notes\n");
        assert_eq!(choose_view(&headers, html), RenderMode::Native);
    }

    #[test]
    fn bulk_but_simple_stays_native() {
        let headers = MailHeaders::from_lines(
            "List-Unsubscribe: <https://example.com/unsub>\nPrecedence: bulk\n",
        );
        let html = "<p>Hello there.</p><p>A short note.</p>";
        assert_eq!(choose_view(&headers, html), RenderMode::Native);
    }

    #[test]
    fn precedence_list_and_layout_uses_the_web_view() {
        let headers = MailHeaders::from_lines("Precedence: List\n");
        let html = include_str!("../fixtures/web-newsletter.html");
        assert_eq!(choose_view(&headers, html), RenderMode::Web);
    }

    #[test]
    fn esp_header_names() {
        assert!(MailHeaders::from_lines("X-SG-EID: abc\n").is_bulk());
        assert!(MailHeaders::from_lines("X-Mailer: Mailchimp Mailer\n").is_bulk());
        assert!(!MailHeaders::from_lines("X-Mailer: Thunderbird\n").is_bulk());
        assert!(!MailHeaders::from_lines("List-Unsubscribe:\n").is_bulk());
    }
}
