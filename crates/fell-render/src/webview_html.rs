use std::ops::Deref;

/// HTML handed to WebKit. Scripts are removed, remote images are stripped
/// until the reader allows them, and a content-security policy blocks
/// everything else.
pub struct WebFrame {
    pub scheme: &'static str,
    pub allow_remote_images: bool,
}

pub fn wrap_for_webview(source_html: &str, frame: &WebFrame) -> String {
    let body = sanitize_html(source_html, frame.allow_remote_images);
    let img_src = if frame.allow_remote_images {
        "img-src data: blob: https: http:;"
    } else {
        "img-src data: blob:;"
    };
    format!(
        r#"<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<meta name="color-scheme" content="{scheme}">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'; {img_src} font-src data:; media-src 'none'; frame-src 'none'; script-src 'none'; connect-src 'none'; object-src 'none'; base-uri 'none'; form-action 'none';">
<style>
  html, body {{ margin: 0; padding: 0; }}
</style>
</head>
<body>{body}</body>
</html>"#,
        scheme = frame.scheme,
        img_src = img_src,
        body = body,
    )
}

pub fn sanitize_html(html: &str, allow_remote_images: bool) -> String {
    let dom = scraper::Html::parse_document(html);
    let mut out = String::new();
    serialize_children(dom.root_element(), &mut out, allow_remote_images);
    out
}

fn serialize_children(node: scraper::ElementRef<'_>, out: &mut String, allow_remote_images: bool) {
    for child in node.children() {
        serialize_node(child, out, allow_remote_images);
    }
}

fn serialize_node(
    node: ego_tree::NodeRef<'_, scraper::node::Node>,
    out: &mut String,
    allow_remote_images: bool,
) {
    match node.value() {
        scraper::node::Node::Text(text) => out.push_str(&escape(text.deref())),
        scraper::node::Node::Element(element) => {
            let name = element.name();
            if matches!(
                name,
                "script"
                    | "iframe"
                    | "object"
                    | "embed"
                    | "link"
                    | "meta"
                    | "base"
                    | "input"
                    | "button"
                    | "textarea"
                    | "form"
                    | "noscript"
                    | "svg"
                    | "head"
                    | "html"
                    | "body"
            ) {
                if matches!(name, "html" | "body" | "head" | "form" | "noscript" | "svg") {
                    for child in node.children() {
                        serialize_node(child, out, allow_remote_images);
                    }
                }
                return;
            }
            if name == "style" {
                let css = node
                    .children()
                    .filter_map(|child| match child.value() {
                        scraper::node::Node::Text(text) => Some(text.deref()),
                        _ => None,
                    })
                    .collect::<String>();
                let css = if allow_remote_images {
                    css
                } else {
                    strip_remote_css_urls(&css)
                };
                out.push_str("<style>");
                out.push_str(&css.replace("</style", ""));
                out.push_str("</style>");
                return;
            }
            out.push('<');
            out.push_str(name);
            for (attr, value) in element.attrs() {
                if !keep_attr(name, attr, value, allow_remote_images) {
                    continue;
                }
                let value = rewrite_attr(name, attr, value, allow_remote_images);
                out.push(' ');
                out.push_str(attr);
                out.push_str("=\"");
                out.push_str(&escape_attr(&value));
                out.push('"');
            }
            out.push('>');
            if !VOID.contains(&name) {
                for child in node.children() {
                    serialize_node(child, out, allow_remote_images);
                }
                out.push_str("</");
                out.push_str(name);
                out.push('>');
            }
        }
        _ => {}
    }
}

const VOID: &[&str] = &["area", "br", "hr", "img", "wbr", "col"];

fn keep_attr(tag: &str, name: &str, value: &str, allow_remote_images: bool) -> bool {
    let name_l = name.to_ascii_lowercase();
    if name_l.starts_with("on") || name_l == "srcdoc" || name_l == "formaction" {
        return false;
    }
    if matches!(
        name_l.as_str(),
        "href" | "src" | "srcset" | "poster" | "background" | "lowsrc" | "dynsrc" | "xlink:href"
    ) {
        let normalized = normalize_resource_url(value).to_ascii_lowercase();
        if normalized.starts_with("javascript:") || normalized.starts_with("vbscript:") {
            return false;
        }
    }
    allow_remote_images || !image_attr_is_remote(tag, &name_l, value)
}

fn rewrite_attr(_tag: &str, name: &str, value: &str, allow_remote_images: bool) -> String {
    if name.eq_ignore_ascii_case("style") && !allow_remote_images {
        return strip_remote_css_urls(value);
    }
    value.to_string()
}

/// `src` on any element, plus the other attributes that load an image.
/// Anchor `href` is a link, not an image, so it stays.
fn image_attr_is_remote(tag: &str, name: &str, value: &str) -> bool {
    match name {
        "src" | "poster" | "background" | "lowsrc" | "dynsrc" => is_remote_url(value),
        "srcset" => srcset_is_remote(value),
        // HTML parses `<image>` as `<img>`, so a remote href on either is an image.
        "href" | "xlink:href" if matches!(tag, "img" | "image" | "use" | "feimage") => {
            is_remote_url(value)
        }
        _ => false,
    }
}

fn srcset_is_remote(value: &str) -> bool {
    value.split(',').any(is_remote_url)
}

fn is_remote_url(value: &str) -> bool {
    let value = normalize_resource_url(value).to_ascii_lowercase();
    value.starts_with("http:")
        || value.starts_with("https:")
        || value.starts_with("//")
        || value.starts_with("file:")
}

/// URL parsers drop newlines and tabs, and CSS hex escapes decode before that.
/// `ht&#10;tps://` and `url(\68 ttp://...)` are both `http://` after this.
fn normalize_resource_url(value: &str) -> String {
    let decoded = decode_html_character_references(value);
    let unescaped = decode_css_escapes(&decoded);
    unescaped
        .chars()
        .filter(|ch| !is_ignorable_url_ws(*ch))
        .collect()
}

fn is_ignorable_url_ws(ch: char) -> bool {
    matches!(ch, ' ' | '\t' | '\n' | '\r' | '\u{000c}' | '\u{000b}')
}

fn decode_html_character_references(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find('&') {
        out.push_str(&rest[..start]);
        let (ch, consumed) = decode_character_reference(&rest[start..]).unwrap_or(('&', 1));
        out.push(ch);
        rest = &rest[start + consumed..];
    }
    out.push_str(rest);
    out
}

fn decode_character_reference(input: &str) -> Option<(char, usize)> {
    let bytes = input.as_bytes();
    if bytes.first() != Some(&b'&') {
        return None;
    }
    if bytes.get(1) == Some(&b'#') {
        let (hex, digit_at) = if matches!(bytes.get(2), Some(b'x' | b'X')) {
            (true, 3)
        } else {
            (false, 2)
        };
        let mut end = digit_at;
        while end < bytes.len() && end - digit_at < 8 {
            let digit = bytes[end];
            let ok = if hex {
                digit.is_ascii_hexdigit()
            } else {
                digit.is_ascii_digit()
            };
            if !ok {
                break;
            }
            end += 1;
        }
        if end == digit_at {
            return None;
        }
        let digits = std::str::from_utf8(&bytes[digit_at..end]).ok()?;
        let code = if hex {
            u32::from_str_radix(digits, 16).ok()?
        } else {
            digits.parse().ok()?
        };
        if bytes.get(end) == Some(&b';') {
            end += 1;
        }
        let ch = match code {
            0 => '\u{FFFD}',
            value => char::from_u32(value).unwrap_or('\u{FFFD}'),
        };
        return Some((ch, end));
    }
    let mut end = 1;
    while end < bytes.len() && bytes[end].is_ascii_alphabetic() && end < 32 {
        end += 1;
    }
    if bytes.get(end) != Some(&b';') {
        return None;
    }
    let name = std::str::from_utf8(&bytes[1..end]).ok()?;
    let ch = match name.to_ascii_lowercase().as_str() {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => '\u{00A0}',
        "newline" => '\n',
        "tab" => '\t',
        _ => return None,
    };
    Some((ch, end + 1))
}

fn decode_css_escapes(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        let Some(next) = chars.peek().copied() else {
            out.push('\\');
            break;
        };
        if next == '\n' || next == '\u{000c}' {
            chars.next();
            continue;
        }
        if next == '\r' {
            chars.next();
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            continue;
        }
        if next.is_ascii_hexdigit() {
            let mut hex = String::new();
            while hex.len() < 6 {
                let Some(digit) = chars.peek().copied() else {
                    break;
                };
                if !digit.is_ascii_hexdigit() {
                    break;
                }
                hex.push(digit);
                chars.next();
            }
            if let Some(whitespace) = chars.peek().copied()
                && is_css_whitespace(whitespace)
            {
                chars.next();
                if whitespace == '\r' && chars.peek() == Some(&'\n') {
                    chars.next();
                }
            }
            let code = u32::from_str_radix(&hex, 16).unwrap_or(0);
            out.push(match code {
                0 => '\u{FFFD}',
                value => char::from_u32(value).unwrap_or('\u{FFFD}'),
            });
            continue;
        }
        out.push(chars.next().expect("peeked"));
    }
    out
}

fn is_css_whitespace(ch: char) -> bool {
    matches!(ch, ' ' | '\t' | '\n' | '\r' | '\u{000c}')
}

pub fn strip_remote_css_urls(css: &str) -> String {
    let mut out = String::with_capacity(css.len());
    let mut rest = css;
    loop {
        let lower = rest.to_ascii_lowercase();
        let Some(idx) = lower.find("url(") else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..idx]);
        let after = &rest[idx + 4..];
        let Some(end) = after.find(')') else {
            out.push_str(&rest[idx..]);
            break;
        };
        let inside = after[..end]
            .trim()
            .trim_matches(|ch| ch == '"' || ch == '\'' || ch == ' ');
        if is_remote_url(inside) {
            out.push_str("url(\"about:blank\")");
        } else {
            out.push_str(&rest[idx..idx + 4 + end + 1]);
        }
        rest = &after[end + 1..];
    }
    out
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn escape_attr(text: &str) -> String {
    escape(text).replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webview_html_strips_script_and_remote_images() {
        let html = r#"<p onclick="alert(1)">Hi</p><script>alert(1)</script><img src="https://cdn.example/a.png" alt="hero"><img src="data:image/png;base64,aaaa" alt="dot"><a href="javascript:alert(1)">x</a>"#;
        let clean = sanitize_html(html, false);
        assert!(!clean.to_ascii_lowercase().contains("<script"));
        assert!(!clean.contains("onclick"));
        assert!(!clean.contains("https://cdn.example"));
        assert!(!clean.contains("javascript:"));
        assert!(clean.contains("data:image/png"));
        assert!(clean.contains("hero"));
        let wrapped = wrap_for_webview(
            html,
            &WebFrame {
                scheme: "dark",
                allow_remote_images: false,
            },
        );
        assert!(wrapped.contains("script-src 'none'"));
        assert!(!wrapped.contains("fell-chrome"));
        assert!(!wrapped.contains("<title>"));
        let unwrapped = sanitize_html(
            "<form><p>Hello</p><input><button>Go</button><textarea>x</textarea></form><noscript><p>Fallback</p></noscript>",
            false,
        );
        assert!(unwrapped.contains("Hello"));
        assert!(unwrapped.contains("Fallback"));
        assert!(!unwrapped.contains("<form"));
        assert!(!unwrapped.contains("<input"));
        assert!(!unwrapped.contains("<button"));
        assert!(!unwrapped.contains("<textarea"));
    }

    #[test]
    fn allowing_images_keeps_https_src() {
        let html = r#"<img src="https://cdn.example/a.png" alt="hero"><table><tr><td background="https://cdn.example/bg.png"></td></tr></table><video poster="https://cdn.example/poster.png"></video>"#;
        let clean = sanitize_html(html, true);
        assert!(clean.contains("https://cdn.example/a.png"));
        assert!(clean.contains("https://cdn.example/bg.png"));
        assert!(clean.contains("https://cdn.example/poster.png"));
    }

    #[test]
    fn remote_image_urls_are_normalized_before_the_check() {
        assert!(is_remote_url("ht\ntps://tracker.example/p.png"));
        assert!(is_remote_url("ht&#10;tps://tracker.example/p.png"));
        assert!(is_remote_url("ht&#x0A;tps://tracker.example/p.png"));
        assert!(is_remote_url("ht&NewLine;tps://tracker.example/p.png"));
        assert!(is_remote_url(r"\68 ttp://tracker.example/p.png"));
        assert!(is_remote_url(r"\000068ttp://tracker.example/p.png"));
        assert!(!is_remote_url("data:image/png;base64,aaaa"));
        assert!(!is_remote_url("notes.png"));

        let html = concat!(
            r#"<img src="ht&#10;tps://tracker.example/a.png" alt="a">"#,
            r#"<img srcset="ht&#10;tps://tracker.example/f.png 1x">"#,
            r#"<table><tr><td background="https://tracker.example/b.png"></td></tr></table>"#,
            r#"<video poster="https://tracker.example/c.png"></video>"#,
            r#"<source src="https://tracker.example/g.png">"#,
            r#"<style>b { background: url(\68 ttp://tracker.example/d.png); }</style>"#,
            r#"<p style="background:url(\68 ttp://tracker.example/e.png)">x</p>"#,
            r#"<image href="https://tracker.example/h.png"></image>"#,
            r#"<a href="https://example.com/read">Read</a>"#,
            r#"<a href="java&#10;script:alert(1)">no</a>"#,
            r#"<img src="data:image/png;base64,aaaa" alt="dot">"#,
            r#"<table><tr><td background="notes.png"></td></tr></table>"#,
        );
        let clean = sanitize_html(html, false);
        assert!(!clean.contains("tracker.example"), "{clean}");
        assert!(
            !clean.to_ascii_lowercase().contains("javascript:"),
            "{clean}"
        );
        assert!(clean.contains("https://example.com/read"), "{clean}");
        assert!(clean.contains("data:image/png"), "{clean}");
        assert!(clean.contains("notes.png"), "{clean}");
        assert!(clean.contains(">x<") || clean.contains(">x</p>"), "{clean}");
    }
}
