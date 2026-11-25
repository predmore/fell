use std::ops::Range;

use url::{Host, Url};

use crate::{Inline, html};

/// Keep the HTML scheme allowlist and require a destination the native
/// reader can show as written. Opaque or internationalized hosts remain
/// visible text.
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
        "mailto" => url.path().split(',').all(|address| {
            address.rsplit_once('@').is_some_and(|(local, domain)| {
                !local.is_empty()
                    && !local.contains('@')
                    && !domain.contains('%')
                    && Host::parse(domain).is_ok_and(ordinary_host)
            })
        }),
        _ => false,
    };
    allowed.then_some(target)
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

/// The byte ranges of links in plain text, in source order.
pub fn plain_link_ranges(text: &str) -> Vec<Range<usize>> {
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
        let end = text[colon + 1..]
            .find(|ch: char| {
                ch.is_whitespace() || matches!(ch, '<' | '>' | '"' | '`' | '“' | '”' | '‘' | '’')
            })
            .map_or(text.len(), |end| colon + 1 + end);
        scanned = end;
        let label = trim_url_end(&text[start..end]);
        if reader_link_target(label).as_deref() == Some(label) {
            links.push(start..start + label.len());
        }
    }
    links
}

pub(crate) fn plain_link_inlines(text: &str) -> Vec<Inline> {
    let mut inlines = Vec::new();
    let mut copied = 0;
    for range in plain_link_ranges(text) {
        if copied < range.start {
            inlines.push(Inline::Text(text[copied..range.start].into()));
        }
        copied = range.end;
        let label = &text[range];
        inlines.push(Inline::Link {
            href: label.into(),
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
            (
                "mailto:help@example.test?Subject=Release&body=A%20note",
                true,
            ),
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
            let doc = html_to_document(&format!("<a href=\"{target}\">link</a>"));
            let [Block::Paragraph { inlines }] = doc.blocks.as_slice() else {
                panic!("{doc:?}");
            };
            let [Inline::Link { href, .. }] = inlines.as_slice() else {
                panic!("{inlines:?}");
            };
            assert_eq!(reader_link_target(href).is_some(), allowed, "{target}");
        }
        check(
            "ftp://www.example.test foohttps://example.test http://",
            &[],
        );
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
