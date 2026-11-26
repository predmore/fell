use std::{borrow::Cow, ops::Range};

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use url::{Host, Url};

use crate::{Inline, html};

/// Keep the HTML scheme allowlist and require a destination the native
/// reader can show. Opaque or internationalized hosts remain visible text.
pub fn reader_link_target(href: &str) -> Option<String> {
    let target = html::safe_href(href);
    if target
        .chars()
        .any(|ch| ch.is_control() || html::is_invisible(ch))
    {
        return None;
    }
    let url = Url::parse(&target).ok()?;
    let (_, rest) = target.split_once(':')?;
    let allowed = match url.scheme() {
        "http" | "https" => {
            let authority = rest.strip_prefix("//")?.split(['/', '?', '#']).next()?;
            !authority.is_empty()
                && authority.is_ascii()
                && !rest.contains('\\')
                && !authority.contains('@')
                && url.host().is_some_and(ordinary_host)
        }
        "mailto" => true, // Already parsed and rebuilt by safe_href.
        _ => false,
    };
    allowed.then_some(target)
}

const URI_COMPONENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// Build the URI the system mail client receives from the visible recipient
/// list and the two supported fields. Mail clients disagree about separators
/// and header names, so never forward the input query verbatim.
pub(crate) fn canonical_mailto(rest: &str) -> Option<String> {
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    let mut target = String::from("mailto:");
    for (index, address) in path.split(',').enumerate() {
        let (raw_local, raw_domain) = address.split_once('@')?;
        let local = decode_mailto_component(raw_local)?;
        let domain = decode_mailto_component(raw_domain)?;
        // Trim only the outside of the mailbox, including encoded padding.
        let local = local.trim_start_matches([' ', '\t']);
        let domain = domain.trim_end_matches([' ', '\t']);
        // A client may decode the path before parsing mailboxes. Accept a
        // local part only if decoding can't introduce mailbox syntax.
        if local.is_empty()
            || local.contains([
                '@', ',', ';', ':', '"', '\'', '\\', '<', '>', '[', ']', '(', ')',
            ])
            || local.chars().any(char::is_whitespace)
            || !domain.bytes().all(|byte| {
                byte.is_ascii_alphanumeric()
                    || matches!(byte, b'.' | b'-' | b'_' | b':' | b'[' | b']')
            })
            || !Host::parse(domain).is_ok_and(ordinary_host)
        {
            return None;
        }
        if index > 0 {
            target.push(',');
        }
        target.extend(utf8_percent_encode(local, URI_COMPONENT));
        target.push('@');
        target.push_str(domain);
    }

    let (mut subject, mut body) = (None, None);
    for field in query.split('&').filter(|field| !field.is_empty()) {
        let parts = field.split_once('=');
        let (key, value) = parts.unwrap_or((field, ""));
        let key = decode_mailto_component(key)?.trim().to_ascii_lowercase();
        let value = decode_mailto_component(value)?;
        let slot = match key.as_str() {
            "subject" => &mut subject,
            "body" => &mut body,
            // Keep rejecting the malformed subject/Bcc spelling as well.
            "to" | "cc" | "bcc" | "from" | "subjectbcc" => return None,
            _ => continue,
        };
        if parts.is_none() || slot.is_some() || value.contains(['\u{2028}', '\u{2029}']) {
            return None;
        }
        *slot = Some(value);
    }

    let mut separator = '?';
    for (key, value) in [("subject", subject), ("body", body)] {
        if let Some(value) = value {
            target.push(separator);
            target.push_str(key);
            target.push('=');
            target.extend(utf8_percent_encode(&value, URI_COMPONENT));
            separator = '&';
        }
    }
    Some(target)
}

fn is_percent_escape(bytes: &[u8]) -> bool {
    matches!(bytes, [b'%', a, b, ..] if a.is_ascii_hexdigit() && b.is_ascii_hexdigit())
}

fn decode_mailto_component(raw: &str) -> Option<Cow<'_, str>> {
    if raw
        .match_indices('%')
        .any(|(at, _)| !is_percent_escape(&raw.as_bytes()[at..]))
    {
        return None;
    }
    let decoded = percent_decode_str(raw).decode_utf8().ok()?;
    // Reject separators some clients interpret after decoding, and a second
    // layer of escapes whose interpretation would depend on the opener.
    if decoded.contains(['#', '|'])
        || decoded
            .chars()
            .any(|ch| (ch.is_control() && ch != '\t') || html::is_invisible(ch))
        || decoded.as_bytes().windows(3).any(is_percent_escape)
    {
        return None;
    }
    Some(decoded)
}

fn ordinary_host<S: AsRef<str>>(host: Host<S>) -> bool {
    match host {
        Host::Domain(domain) => !domain
            .as_ref()
            .split('.')
            .any(|label| label.starts_with("xn--")),
        Host::Ipv4(_) | Host::Ipv6(_) => true,
    }
}

/// The label ranges and checked targets of links in plain text, in source order.
pub fn plain_link_targets(text: &str) -> Vec<(Range<usize>, String)> {
    let mut links = Vec::new();
    let mut scanned = 0;
    for (colon, _) in text.match_indices(':') {
        if colon < scanned {
            continue;
        }
        let Some(start) = ["https", "http", "mailto"].into_iter().find_map(|scheme| {
            let start = colon.checked_sub(scheme.len())?;
            text.get(start..colon)?
                .eq_ignore_ascii_case(scheme)
                .then_some(start)
        }) else {
            continue;
        };
        if text[..start].chars().next_back().is_some_and(|ch| {
            !ch.is_whitespace()
                && !matches!(
                    ch,
                    '(' | '[' | '{' | '<' | '>' | '"' | '\'' | '`' | '“' | '‘'
                )
        }) {
            continue;
        }
        let rest = &text[colon + 1..];
        let end = colon
            + 1
            + if text[start..colon].eq_ignore_ascii_case("mailto") {
                plain_mailto_end(rest)
            } else {
                plain_url_end(rest)
            };
        scanned = end;
        let label = trim_url_end(&text[start..end]);
        if let Some(target) = reader_link_target(label)
            && (target == label || target.starts_with("mailto:"))
        {
            links.push((start..start + label.len(), target));
        }
    }
    links
}

fn plain_url_end(text: &str) -> usize {
    text.find(|ch: char| {
        ch.is_whitespace() || matches!(ch, '<' | '>' | '"' | '`' | '“' | '”' | '‘' | '’')
    })
    .unwrap_or(text.len())
}

fn plain_mailto_end(text: &str) -> usize {
    let mut end = plain_url_end(text);
    let mut segment = &text[..end];
    while !segment.contains('?') {
        let tail = &text[end..];
        let next = tail.trim_start_matches([' ', '\t']);
        if next.len() == tail.len() {
            break;
        }
        let next = match next.strip_prefix(',') {
            Some(rest) => rest.trim_start_matches([' ', '\t']),
            None if next.starts_with('?') || segment.ends_with(',') => next,
            None => break,
        };
        let next_segment = &next[..plain_url_end(next)];
        let path = next_segment
            .split_once('?')
            .map_or(next_segment, |(path, _)| path);
        // A comma can end the link in prose. Consume an address-shaped
        // continuation so validation can reject the entire target if needed.
        if !next_segment.starts_with('?')
            && path
                .split_once('@')
                .is_none_or(|(local, _)| local.contains(':'))
        {
            break;
        }
        segment = next_segment;
        end = text.len() - next.len() + segment.len();
    }
    end
}

pub(crate) fn plain_link_inlines(text: &str) -> Vec<Inline> {
    let mut inlines = Vec::new();
    let mut copied = 0;
    for (range, href) in plain_link_targets(text) {
        if copied < range.start {
            inlines.push(Inline::Text(text[copied..range.start].into()));
        }
        copied = range.end;
        let label = &text[range];
        inlines.push(Inline::Link {
            href,
            children: vec![Inline::Text(label.into())],
        });
    }
    if copied < text.len() || inlines.is_empty() {
        inlines.push(Inline::Text(text[copied..].into()));
    }
    inlines
}

fn trim_url_end(candidate: &str) -> &str {
    let mut openings = [0usize; 3];
    let mut end = 0;
    for (at, ch) in candidate.char_indices() {
        let bracket = match ch {
            '(' | ')' => Some(0),
            '[' | ']' => Some(1),
            '{' | '}' => Some(2),
            _ => None,
        };
        if let Some(bracket) = bracket {
            if matches!(ch, '(' | '[' | '{') {
                openings[bracket] += 1;
            } else if openings[bracket] == 0 {
                break;
            } else {
                openings[bracket] -= 1;
            }
        }
        if !matches!(
            ch,
            '.' | ',' | '!' | '?' | ':' | ';' | '\'' | '…' | '。' | '，' | '！' | '？'
        ) {
            end = at + ch.len_utf8();
        }
    }
    &candidate[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Block, StyledDocument, html_to_document};

    fn check(text: &str, expected: &[&str]) {
        let inlines = plain_link_inlines(text);
        let targets: Vec<_> = inlines
            .iter()
            .filter_map(|inline| match inline {
                Inline::Link { href, children } => {
                    assert_eq!(children, &[Inline::Text(href.clone())]);
                    Some(href.as_str())
                }
                _ => None,
            })
            .collect();
        assert_eq!(targets, expected);
        let doc = StyledDocument {
            blocks: vec![Block::Paragraph { inlines }],
        };
        assert_eq!(doc.plain_text(), text);
    }

    #[test]
    fn keeps_punctuation_and_hard_breaks_outside_links() {
        check(
            "See (https://example.test/Book_(chapter)), [http://example.test/a].\n> <HTTPS://example.test/b?x=1&y=2>!",
            &[
                "https://example.test/Book_(chapter)",
                "http://example.test/a",
                "HTTPS://example.test/b?x=1&y=2",
            ],
        );
        check(
            "Quotes: 'https://example.test/o' and https://[::1]/x[1]}.",
            &["https://example.test/o", "https://[::1]/x[1]"],
        );
    }

    #[test]
    fn plaintext_and_html_use_the_same_destination_checks() {
        for (target, allowed) in [
            ("https://example.test/café", true),
            ("https://bücher.example/path", false),
            ("https://ｅxample.test/path", false),
            ("https://xn--bcher-kva.example/path", false),
            ("https://example.test@elsewhere.test/path", false),
            ("mailto:help@example.test?subject=Release", true),
            ("mailto:help@example.test?to=other@example.test", false),
            ("mailto:help@example.test?Cc=other@example.test", false),
            (
                "mailto:help@example.test?subject=Hi&%62cC=other@example.test",
                false,
            ),
            (
                "mailto:help@example.test?subject=Hi%0ABcc:other@example.test",
                false,
            ),
            ("mailto:help@example.test?body=Hi%0dthere", false),
            ("mailto:help@example.test?s%0Dubject=Hi", false),
            ("mailto:help@bücher.example", false),
            ("mailto:help@example.test,help@xn--bcher-kva.example", false),
            ("https:///example.test", false),
            ("javascript:alert(1)", false),
            ("file:///tmp/file", false),
            ("ftp://example.test", false),
            ("www.example.test/path", false),
            ("help@example.test", false),
        ] {
            check(
                target,
                if allowed {
                    std::slice::from_ref(&target)
                } else {
                    &[]
                },
            );
            assert_eq!(
                reader_link_target(&html_href(target)).is_some(),
                allowed,
                "{target}"
            );
        }
        check(
            "ftp://www.example.test foohttps://example.test http://",
            &[],
        );
    }

    fn html_href(target: &str) -> String {
        let target = target.replace('&', "&amp;");
        let doc = html_to_document(&format!("<a href=\"{target}\">link</a>"));
        assert_eq!(doc.plain_text(), "link");
        let [Block::Paragraph { inlines }] = doc.blocks.as_slice() else {
            panic!("{doc:?}");
        };
        let [Inline::Link { href, .. }] = inlines.as_slice() else {
            panic!("{inlines:?}");
        };
        href.clone()
    }

    #[test]
    fn mailto_is_rebuilt_from_the_recipients_and_supported_fields() {
        for (raw, expected) in [
            (
                "mailto:a@x,b@y?subject=Hi&body=Text",
                "mailto:a@x,b@y?subject=Hi&body=Text",
            ),
            (
                "mailto:ada@example.com, bea@example.com?subject=Hello",
                "mailto:ada@example.com,bea@example.com?subject=Hello",
            ),
            ("mailto:a@x,%20b@y?subject=Hi", "mailto:a@x,b@y?subject=Hi"),
            (
                "mailto:a@x , b@y,\tc@z ?subject=Hi",
                "mailto:a@x,b@y,c@z?subject=Hi",
            ),
            (
                "MAILTO:a%2Bb@x?body=Note&%20Subject%09=Hello%20world&priority=high",
                "mailto:a%2Bb@x?subject=Hello%20world&body=Note",
            ),
            (
                "mailto:a@x?subject=100%25+done&body=A%26B%3DC%3F",
                "mailto:a@x?subject=100%25%2Bdone&body=A%26B%3DC%3F",
            ),
            (
                "mailto:a@x?reply-to=b@y&in-reply-to=id&subject=Hi",
                "mailto:a@x?subject=Hi",
            ),
            ("mailto:a@x?priority=high", "mailto:a@x"),
        ] {
            let inlines = plain_link_inlines(raw);
            assert_eq!(
                inlines,
                [Inline::Link {
                    href: expected.into(),
                    children: vec![Inline::Text(raw.into())],
                }]
            );
            let html = html_href(raw);
            assert_eq!(html, expected);
            assert_eq!(reader_link_target(&html).as_deref(), Some(expected));
        }
    }

    #[test]
    fn spaced_mailto_continuations_preserve_prose_and_validate_the_whole_target() {
        check("mailto:a@x, then call me", &["mailto:a@x"]);
        check(
            "mailto:a@x, https://example.test/u@team",
            &["mailto:a@x", "https://example.test/u@team"],
        );
        check("mailto:a@x, b@bücher.example?subject=Hi", &[]);
        check("mailto:a@x ?bcc=other@example.test", &[]);
        check("mailto:a@x, b@y?to=other@example.test", &[]);
        check("mailto:a@x,\nb@y?subject=Hi", &["mailto:a@x"]);
    }

    #[test]
    fn ambiguous_mailto_targets_remain_text() {
        for rest in [
            "help@example.test?subject=Hi#&bcc=other@example.test",
            "help@example.test#&bcc=other@example.test",
            "help@example.test?subject=Hi|bcc=other@example.test",
            "help@example.test?subject=Hi%7Cbcc=other@example.test",
            "help@example.test?subject=Hi%257Cbcc=other@example.test",
            "help@example.test?to%20=other@example.test",
            "help@example.test?bcc%20=other@example.test",
            "help@example.test?cc%20=other@example.test",
            "help@example.test?bcc%09=other@example.test",
            "help@example.test?%62cc%20=other@example.test",
            "help@example.test?%62cc=other@example.test",
            "help@example.test?subjectbcc=other@example.test",
            "help@example.test?from=other@example.test",
            "help@example.test?subject=one&Subject=two",
            "help@example.test?body=broken%ZZ",
            "help%40example.test%2Cother@example.test",
            "help%20@example.test",
            "help@%20example.test",
        ] {
            let target = format!("mailto:{rest}");
            assert_eq!(reader_link_target(&target), None, "{target}");
            assert_eq!(plain_link_inlines(&target), [Inline::Text(target.clone())]);
            assert!(html_href(&target).is_empty(), "{target}");
        }
    }

    #[test]
    fn raw_line_breaks_in_mailto_queries_are_rejected_before_url_cleanup() {
        for (ch, entity) in [('\r', "&#13;"), ('\n', "&#10;")] {
            let target = format!("mailto:help@example.test?subject=Hi{ch}there");
            assert_eq!(reader_link_target(&target), None);
            let trailing = format!("mailto:help@example.test?subject=Hi{ch}");
            assert_eq!(reader_link_target(&trailing), None);

            let doc = html_to_document(&format!(
                "<a href=\"mailto:help@example.test?subject=Hi{entity}there\">link</a>"
            ));
            let [Block::Paragraph { inlines }] = doc.blocks.as_slice() else {
                panic!("{doc:?}");
            };
            let [Inline::Link { href, .. }] = inlines.as_slice() else {
                panic!("{inlines:?}");
            };
            assert!(href.is_empty());
        }
    }

    #[test]
    fn unmatched_closers_and_backticks_end_links() {
        check(
            "https://example.test/notes).Next https://example.test/a)b https://example.test/x]y https://example.test/u}v `https://example.test/a`",
            &[
                "https://example.test/notes",
                "https://example.test/a",
                "https://example.test/x",
                "https://example.test/u",
                "https://example.test/a",
            ],
        );
    }

    #[test]
    fn plain_list_items_and_continuations_keep_links() {
        let text = "-   Start with https://example.test/start.\n    Continue at <https://example.test/next>.\n-   Done";
        let doc = crate::plain_to_document(text);
        let [Block::List { items, .. }] = doc.blocks.as_slice() else {
            panic!("{doc:?}");
        };
        let first: Vec<_> = items[0]
            .blocks
            .iter()
            .flat_map(|block| match block {
                Block::Paragraph { inlines } => inlines.as_slice(),
                _ => &[],
            })
            .filter_map(|inline| match inline {
                Inline::Link { href, .. } => Some(href.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            first,
            ["https://example.test/start", "https://example.test/next"]
        );
    }
}
