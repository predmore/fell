use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::ops::Deref;

fn web_page(body: &str, scheme: &str) -> String {
    format!(
        r#"<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<meta name="color-scheme" content="{scheme}">
<meta name="referrer" content="no-referrer">
<meta http-equiv="x-dns-prefetch-control" content="off">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'; img-src data: cid:; font-src data:; media-src 'none'; frame-src 'none'; script-src 'none'; connect-src 'none'; object-src 'none'; base-uri 'none'; form-action 'none';">
<style>
  html, body {{ margin: 0; padding: 0; background: transparent; }}
  table, img {{ max-width: 100%; }}
  body {{ overflow-wrap: anywhere; }}
  img[src=""] {{ background-color: rgba(128, 128, 128, 0.12); }}
  img {{ object-fit: contain; }}
</style>
</head>
<body>{body}</body>
</html>"#,
        scheme = scheme,
        body = body,
    )
}

/// A sanitized page with slots for remote raster images. The browser only ever
/// sees local bytes. The caller supplies a slot value after the user consents.
#[derive(Debug)]
pub struct WebMail {
    body: String,
    images: Vec<(usize, String)>,
}

impl WebMail {
    pub fn prepare(html: &str) -> Self {
        let dom = crate::html::parse_html(html);
        let mut body = String::new();
        let mut images = Vec::new();
        serialize_children(
            dom.root_element(),
            &mut body,
            false,
            true,
            0,
            Some(&mut images),
        );
        Self { body, images }
    }

    pub fn has_remote_images(&self) -> bool {
        !self.images.is_empty()
    }

    pub fn page(&self, scheme: &str, mut image: impl FnMut(&str) -> Option<String>) -> String {
        let mut body = String::with_capacity(self.body.len());
        let mut offset = 0;
        for (at, url) in &self.images {
            body.push_str(&self.body[offset..*at]);
            if let Some(data) = image(url).filter(|data| data.starts_with("data:image/")) {
                body.push_str(&escape_attr(&data));
            }
            offset = *at;
        }
        body.push_str(&self.body[offset..]);
        web_page(&body, scheme)
    }
}

pub fn sanitize_html(html: &str, allow_remote_images: bool) -> String {
    let dom = crate::html::parse_html(html);
    let mut out = String::new();
    serialize_children(
        dom.root_element(),
        &mut out,
        allow_remote_images,
        true,
        0,
        None,
    );
    out
}

fn serialize_children(
    node: scraper::ElementRef<'_>,
    out: &mut String,
    allow_remote_images: bool,
    allow_style: bool,
    depth: u32,
    mut images: Option<&mut Vec<(usize, String)>>,
) {
    for child in node.children() {
        serialize_node(
            child,
            out,
            allow_remote_images,
            allow_style,
            depth + 1,
            images.as_deref_mut(),
        );
    }
}

fn serialize_node(
    node: ego_tree::NodeRef<'_, scraper::node::Node>,
    out: &mut String,
    allow_remote_images: bool,
    allow_style: bool,
    depth: u32,
    mut images: Option<&mut Vec<(usize, String)>>,
) {
    if depth >= crate::html::MAX_HTML_DEPTH {
        out.push_str(&escape(&subtree_text(node)));
        return;
    }
    match node.value() {
        scraper::node::Node::Text(text) => out.push_str(&escape(text.deref())),
        scraper::node::Node::Element(element) => {
            let name = element.name();
            if name == "iframe" {
                // Fallback markup is raw text. Parse it and keep the children.
                // The iframe tag and its src are not emitted.
                let fallback = subtree_text(node);
                let parsed = crate::html::parse_html(&fallback);
                serialize_children(
                    parsed.root_element(),
                    out,
                    allow_remote_images,
                    allow_style,
                    depth,
                    images,
                );
                return;
            }
            if matches!(
                name,
                "script"
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
                    | "head"
                    | "html"
                    | "body"
            ) {
                if matches!(
                    name,
                    "html" | "body" | "head" | "form" | "noscript" | "object"
                ) {
                    for child in node.children() {
                        serialize_node(
                            child,
                            out,
                            allow_remote_images,
                            allow_style,
                            depth + 1,
                            images.as_deref_mut(),
                        );
                    }
                }
                return;
            }
            if name == "style" {
                if !allow_style {
                    return;
                }
                let css = node
                    .children()
                    .filter_map(|child| match child.value() {
                        scraper::node::Node::Text(text) => Some(text.deref()),
                        _ => None,
                    })
                    .collect::<String>();
                out.push_str("<style>");
                if let Some(slots) = images.as_deref_mut() {
                    serialize_css(&css, out, slots, escape_style);
                } else {
                    out.push_str(&escape_style(&sanitize_css(&css, allow_remote_images)));
                }
                out.push_str("</style>");
                return;
            }
            out.push('<');
            out.push_str(name);
            for (attr, value) in element.attrs() {
                // Only image attributes and image-valued CSS declarations
                // may enter the fetch list.
                if let Some(slots) = images.as_deref_mut() {
                    if (attr == "src" && name == "img"
                        || attr == "background" && matches!(name, "table" | "tr" | "td" | "th"))
                        && let Some(url) = remote_image_url(value)
                    {
                        if slots.len() < 48 {
                            out.push(' ');
                            out.push_str(attr);
                            out.push_str("=\"");
                            slots.push((out.len(), url));
                            out.push('"');
                        }
                        continue;
                    }
                    if attr == "style" {
                        out.push_str(" style=\"");
                        serialize_css(value, out, slots, escape_attr);
                        out.push('"');
                        continue;
                    }
                    if matches!(attr, "ping" | "attributionsrc") {
                        continue;
                    }
                }
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
                let child_style = allow_style && name != "svg" && name != "math";
                for child in node.children() {
                    serialize_node(
                        child,
                        out,
                        allow_remote_images,
                        child_style,
                        depth + 1,
                        images.as_deref_mut(),
                    );
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

fn subtree_text(node: ego_tree::NodeRef<'_, scraper::node::Node>) -> String {
    let mut text = String::new();
    for descendant in node.descendants() {
        if let scraper::node::Node::Text(value) = descendant.value() {
            text.push_str(value.deref());
        }
    }
    text
}

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
    if image_attr_is_blocked(tag, &name_l, value, allow_remote_images) {
        return false;
    }
    allow_remote_images || !image_attr_is_remote(tag, &name_l, value)
}

fn rewrite_attr(tag: &str, name: &str, value: &str, allow_remote_images: bool) -> String {
    if name.eq_ignore_ascii_case("style") {
        return sanitize_css(value, allow_remote_images);
    }
    if name.eq_ignore_ascii_case("srcset") {
        return value
            .split(',')
            .filter(|part| !image_attr_is_blocked(tag, "srcset", part, allow_remote_images))
            .collect::<Vec<_>>()
            .join(",");
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

fn remote_image_url(value: &str) -> Option<String> {
    let value = value.trim();
    if value.len() > 4096 {
        return None;
    }
    let url = if value.starts_with("//") {
        url::Url::parse(&format!("https:{value}")).ok()?
    } else {
        url::Url::parse(value).ok()?
    };
    (matches!(url.scheme(), "http" | "https")
        && url.username().is_empty()
        && url.password().is_none()
        && !targets_private_network(url.as_str()))
    .then(|| url.into())
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
    sanitize_css(css, false)
}

/// Decode CSS escapes first, then drop remote `@import` and any `url(` the
/// reader is not allowed to fetch. A private or link-local host is never fetched.
fn sanitize_css(css: &str, allow_remote_images: bool) -> String {
    let decoded = decode_css_escapes(css);
    let without_import = strip_css_imports(&decoded, allow_remote_images);
    strip_css_urls(&without_import, allow_remote_images)
}

fn serialize_css(
    css: &str,
    out: &mut String,
    images: &mut Vec<(usize, String)>,
    escape: impl Fn(&str) -> String,
) {
    let css = strip_css_imports(&decode_css_escapes(css), false);
    let lower = css.to_ascii_lowercase();
    let mut from = 0;
    let mut search = 0;
    while let Some(relative) = lower[search..].find("url(") {
        let start = search + relative;
        let url_start = start + 4;
        let Some(end) = css[url_start..].find(')').map(|i| i + url_start) else {
            break;
        };
        search = end + 1;
        let property = lower[..start]
            .rsplit([';', '{', '}'])
            .next()
            .unwrap_or_default();
        let name = property.split_once(':').map(|(name, _)| name.trim());
        if !matches!(
            name,
            Some("background" | "background-image" | "border-image" | "list-style-image")
        ) {
            continue;
        }
        let value = css[url_start..end].trim().trim_matches(['"', '\'']);
        let Some(url) = remote_image_url(value) else {
            continue;
        };
        if images.len() >= 48 {
            break;
        }
        out.push_str(&escape(&strip_css_urls(&css[from..start], false)));
        out.push_str(&escape("url(\""));
        images.push((out.len(), url));
        out.push_str(&escape("\")"));
        from = end + 1;
    }
    out.push_str(&escape(&strip_css_urls(&css[from..], false)));
}

fn strip_css_imports(css: &str, allow_remote_images: bool) -> String {
    let mut out = String::with_capacity(css.len());
    let mut rest = css;
    loop {
        let lower = rest.to_ascii_lowercase();
        let Some(idx) = lower.find("@import") else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..idx]);
        let after = &rest[idx + "@import".len()..];
        let Some(semi) = after.find(';') else {
            out.push_str(&rest[idx..]);
            break;
        };
        let drop = import_target(&after[..semi])
            .is_some_and(|target| resource_is_disallowed(&target, allow_remote_images));
        if !drop {
            out.push_str(&rest[idx..idx + "@import".len() + semi + 1]);
        }
        rest = &after[semi + 1..];
    }
    out
}

fn import_target(prelude: &str) -> Option<String> {
    let prelude = prelude.trim();
    let lower = prelude.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("url(") {
        let end = rest.find(')')?;
        let inside = rest[..end]
            .trim()
            .trim_matches(|ch| ch == '"' || ch == '\'' || ch == ' ');
        return Some(inside.to_string());
    }
    let quote = prelude.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let end = prelude[1..].find(quote)?;
    Some(prelude[1..1 + end].to_string())
}

fn strip_css_urls(css: &str, allow_remote_images: bool) -> String {
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
        if resource_is_disallowed(inside, allow_remote_images) {
            out.push_str("url(\"about:blank\")");
        } else {
            out.push_str(&rest[idx..idx + 4 + end + 1]);
        }
        rest = &after[end + 1..];
    }
    out
}

fn image_attr_is_blocked(tag: &str, name: &str, value: &str, allow_remote_images: bool) -> bool {
    match name {
        "src" | "poster" | "background" | "lowsrc" | "dynsrc" => {
            resource_is_disallowed(value, allow_remote_images)
        }
        "srcset" => value
            .split(',')
            .any(|part| resource_is_disallowed(part, allow_remote_images)),
        "href" | "xlink:href" if matches!(tag, "img" | "image" | "use" | "feimage") => {
            resource_is_disallowed(value, allow_remote_images)
        }
        _ => false,
    }
}

/// A remote image the reader has not allowed, or any loopback, private, or
/// link-local host. The web view is never given the latter.
fn resource_is_disallowed(value: &str, allow_remote_images: bool) -> bool {
    if targets_private_network(value) {
        return true;
    }
    !allow_remote_images && is_remote_url(value)
}

#[cfg(test)]
pub fn request_is_blocked_with(uri: &str, resolve: impl Fn(&str) -> Vec<IpAddr>) -> bool {
    let Ok(url) = url::Url::parse(uri) else {
        return false;
    };
    if url.scheme() != "http" && url.scheme() != "https" {
        return false;
    }
    match url.host() {
        Some(url::Host::Ipv4(ip)) => ipv4_is_blocked(ip),
        Some(url::Host::Ipv6(ip)) => ipv6_is_blocked(ip),
        Some(url::Host::Domain(name)) => {
            let addresses = resolve(name);
            addresses.is_empty() || addresses.into_iter().any(address_is_blocked)
        }
        None => false,
    }
}

pub fn address_is_blocked(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => ipv4_is_blocked(ip),
        IpAddr::V6(ip) => ipv6_is_blocked(ip),
    }
}

fn ipv4_is_blocked(ip: Ipv4Addr) -> bool {
    let [first, second, _, _] = ip.octets();
    first == 0
        || first == 10
        || first == 127
        || (first == 100 && (64..=127).contains(&second))
        || (first == 169 && second == 254)
        || (first == 172 && (16..=31).contains(&second))
        || (first == 192 && second == 168)
}

fn ipv6_is_blocked(ip: Ipv6Addr) -> bool {
    if ip.is_unspecified() || ip.is_loopback() {
        return true;
    }
    if let Some(mapped) = ip.to_ipv4_mapped() {
        return ipv4_is_blocked(mapped);
    }
    // IPv4-compatible ::a.b.c.d. The mapped form was handled above.
    if ip.to_ipv4().is_some() {
        return true;
    }
    let segments = ip.segments();
    let [first, second, ..] = ip.octets();
    // fe80::/10 link-local, fc00::/7 unique-local.
    if (first == 0xfe && (second & 0xc0) == 0x80) || (first & 0xfe) == 0xfc {
        return true;
    }
    // NAT64 64:ff9b::/96 and 6to4 2002::/16.
    (segments[0] == 0x64
        && segments[1] == 0xff9b
        && segments[2] == 0
        && segments[3] == 0
        && segments[4] == 0
        && segments[5] == 0)
        || segments[0] == 0x2002
}

/// `true` when the URL names a loopback, private, or link-local host.
pub fn targets_private_network(value: &str) -> bool {
    let Some(host) = resource_host(value) else {
        return false;
    };
    host_is_private(&host)
}

fn resource_host(value: &str) -> Option<String> {
    let normalized = normalize_resource_url(value).to_ascii_lowercase();
    let rest = normalized
        .strip_prefix("https://")
        .or_else(|| normalized.strip_prefix("http://"))
        .or_else(|| normalized.strip_prefix("//"))?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let hostport = authority.rsplit('@').next().unwrap_or(authority);
    let host = if let Some(rest) = hostport.strip_prefix('[') {
        rest.split(']').next()?.to_string()
    } else {
        hostport.split(':').next()?.to_string()
    };
    let host = host.trim_end_matches('.').to_string();
    (!host.is_empty()).then_some(host)
}

fn host_is_private(host: &str) -> bool {
    let host = percent_decode(host);
    if host == "localhost" || host.ends_with(".localhost") {
        return true;
    }
    if let Some(octets) = parse_ipv4_url(&host) {
        return ipv4_is_private(octets);
    }
    if host.contains(':')
        && let Some(groups) = parse_ipv6(&host)
    {
        return ipv6_is_private(groups);
    }
    false
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && bytes[index + 1].is_ascii_hexdigit()
            && bytes[index + 2].is_ascii_hexdigit()
        {
            let hex = &bytes[index + 1..index + 3];
            let value =
                u8::from_str_radix(std::str::from_utf8(hex).unwrap_or("00"), 16).unwrap_or(0);
            out.push(value);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// [IPv4 parser](https://url.spec.whatwg.org/#concept-ipv4-parser): one to four
/// parts, a trailing dot ignored, `0x` hex, and the last part filling the rest.
fn parse_ipv4_url(host: &str) -> Option<[u8; 4]> {
    let mut parts: Vec<&str> = host.split('.').collect();
    if parts.last() == Some(&"") {
        parts.pop();
    }
    if parts.is_empty() || parts.len() > 4 || parts.iter().any(|part| part.is_empty()) {
        return None;
    }
    let mut numbers = Vec::with_capacity(parts.len());
    for part in &parts {
        numbers.push(parse_ipv4_number(part)?);
    }
    let len = numbers.len();
    if numbers[..len - 1].iter().any(|number| *number > 255) {
        return None;
    }
    let limit = 256u64.pow(u32::try_from(5 - len).unwrap_or(4));
    if numbers[len - 1] >= limit {
        return None;
    }
    let mut ipv4 = numbers[len - 1];
    for (counter, number) in numbers[..len - 1].iter().enumerate() {
        let shift = u32::try_from(3 - counter).unwrap_or(0);
        ipv4 += number * 256u64.pow(shift);
    }
    Some([
        ((ipv4 >> 24) & 0xff) as u8,
        ((ipv4 >> 16) & 0xff) as u8,
        ((ipv4 >> 8) & 0xff) as u8,
        (ipv4 & 0xff) as u8,
    ])
}

fn parse_ipv4_number(input: &str) -> Option<u64> {
    if input.is_empty() {
        return None;
    }
    let (radix, digits) = if input.len() >= 2
        && (input.as_bytes()[0] == b'0')
        && (input.as_bytes()[1] == b'x' || input.as_bytes()[1] == b'X')
    {
        (16, &input[2..])
    } else {
        (10, input)
    };
    if digits.is_empty() || !digits.chars().all(|ch| ch.is_digit(radix)) {
        return None;
    }
    u64::from_str_radix(digits, radix).ok()
}

fn ipv4_is_private(octets: [u8; 4]) -> bool {
    matches!(
        octets,
        [0, ..] | [10, ..] | [127, ..] | [169, 254, ..] | [172, 16..=31, ..] | [192, 168, ..]
    )
}

fn parse_ipv6(host: &str) -> Option<[u16; 8]> {
    if host.matches("::").count() > 1 {
        return None;
    }
    let (left, right) = if let Some((left, right)) = host.split_once("::") {
        (left, Some(right))
    } else {
        (host, None)
    };
    let left_groups = ipv6_pieces(left)?;
    let right_groups = if let Some(right) = right {
        ipv6_pieces(right)?
    } else {
        Vec::new()
    };
    if right.is_none() {
        if left_groups.len() != 8 {
            return None;
        }
        let mut groups = [0u16; 8];
        groups.copy_from_slice(&left_groups);
        return Some(groups);
    }
    if left_groups.len() + right_groups.len() > 8 {
        return None;
    }
    let zeros = 8 - left_groups.len() - right_groups.len();
    let mut groups = [0u16; 8];
    for (index, group) in left_groups.iter().enumerate() {
        groups[index] = *group;
    }
    for (index, group) in right_groups.iter().enumerate() {
        groups[left_groups.len() + zeros + index] = *group;
    }
    Some(groups)
}

fn ipv6_pieces(part: &str) -> Option<Vec<u16>> {
    if part.is_empty() {
        return Some(Vec::new());
    }
    let mut pieces: Vec<&str> = part.split(':').collect();
    let mut groups = Vec::new();
    if pieces.last().is_some_and(|piece| piece.contains('.')) {
        let ipv4 = parse_ipv4_url(pieces.pop()?)?;
        for piece in &pieces {
            groups.push(hextet(piece)?);
        }
        groups.push(u16::from(ipv4[0]) << 8 | u16::from(ipv4[1]));
        groups.push(u16::from(ipv4[2]) << 8 | u16::from(ipv4[3]));
        return Some(groups);
    }
    for piece in pieces {
        groups.push(hextet(piece)?);
    }
    Some(groups)
}

fn hextet(piece: &str) -> Option<u16> {
    if piece.is_empty() || piece.len() > 4 {
        return None;
    }
    u16::from_str_radix(piece, 16).ok()
}

fn ipv6_is_private(groups: [u16; 8]) -> bool {
    if groups == [0, 0, 0, 0, 0, 0, 0, 0] || groups == [0, 0, 0, 0, 0, 0, 0, 1] {
        return true;
    }
    if groups[0] & 0xffc0 == 0xfe80 || groups[0] & 0xfe00 == 0xfc00 {
        return true;
    }
    if groups[..5] == [0, 0, 0, 0, 0] && groups[5] == 0xffff {
        let octets = [
            (groups[6] >> 8) as u8,
            groups[6] as u8,
            (groups[7] >> 8) as u8,
            groups[7] as u8,
        ];
        return ipv4_is_private(octets);
    }
    false
}

/// `<` would close the style element. A CSS escape keeps the stylesheet and
/// cannot smuggle `</STYLE>` or an `<img>` into the document.
fn escape_style(css: &str) -> String {
    css.replace('<', "\\3C ")
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
        let wrapped = WebMail::prepare(html).page("dark", |_| None);
        assert!(wrapped.contains("script-src 'none'"));
        assert!(!wrapped.contains("fell-chrome"));
        assert!(!wrapped.contains("<title>"));
        let unwrapped = sanitize_html(
            "<form><p>Hello</p><input><button>Go</button><textarea>x</textarea></form><noscript><p>Fallback</p></noscript>",
            false,
        );
        assert!(unwrapped.contains("Hello"));
        assert!(unwrapped.contains("<p>Fallback</p>"), "{unwrapped}");
        assert!(!unwrapped.contains("&lt;p&gt;"), "{unwrapped}");
        assert!(!unwrapped.contains("<form"));
        assert!(!unwrapped.contains("<input"));
        assert!(!unwrapped.contains("<button"));
        assert!(!unwrapped.contains("<textarea"));
        let fallback = sanitize_html(
            r#"<iframe src="https://example.com/x"><p>Thursday at 3pm</p></iframe><object data="https://example.com/y"><p>Friday at 4</p></object>"#,
            true,
        );
        assert!(fallback.contains("<p>Thursday at 3pm</p>"), "{fallback}");
        assert!(fallback.contains("<p>Friday at 4</p>"), "{fallback}");
        assert!(!fallback.contains("<iframe"), "{fallback}");
        assert!(!fallback.contains("<object"), "{fallback}");
        assert!(!fallback.contains("example.com"), "{fallback}");
    }

    #[test]
    fn only_image_locations_enter_the_explicit_fetch_path() {
        let mail = WebMail::prepare(
            r#"
           <style>@import url(https://example.test/style);
             @font-face { font-family: test; src: url(https://example.test/font); }
             p { background: url(https://example.test/background); }</style>
           <link rel=prefetch href=https://example.test/prefetch>
           <p><img src=https://example.test/hero><a href=https://example.test/link>Link</a></p>
        "#,
        );
        assert!(mail.has_remote_images());
        let blocked = mail.page("light", |_| None);
        assert!(!blocked.contains("example.test/hero"));
        assert!(!blocked.contains("example.test/background"));
        let mut urls = Vec::new();
        let allowed = mail.page("light", |url| {
            urls.push(url.to_owned());
            Some("data:image/png;base64,AAAA".into())
        });
        assert_eq!(
            urls,
            [
                "https://example.test/background",
                "https://example.test/hero"
            ]
        );
        assert_eq!(allowed.matches("data:image/png").count(), 2);
        assert!(allowed.contains("img-src data: cid:"));
    }

    #[test]
    fn allowing_images_keeps_https_src() {
        let html = r#"<img src="https://cdn.example/a.png" alt="hero"><table><tr><td background="https://cdn.example/bg.png"></td></tr></table><video poster="https://cdn.example/poster.png"></video>"#;
        let clean = sanitize_html(html, true);
        assert!(clean.contains("https://cdn.example/a.png"));
        assert!(clean.contains("https://cdn.example/bg.png"));
        assert!(clean.contains("https://cdn.example/poster.png"));
        let local = sanitize_html(
            r#"<img src="http://127.0.0.1/x" alt="a"><img src="http://169.254.169.254/latest" alt="b"><img src="http://10.1.2.3/c" alt="c"><img src="http://[::1]/d" alt="d"><style>@import "http://192.168.0.1/e.css"; i { background: \75 rl(http://172.16.0.1/f.png); }</style>"#,
            true,
        );
        assert!(!local.contains("127.0.0.1"), "{local}");
        assert!(!local.contains("169.254.169.254"), "{local}");
        assert!(!local.contains("10.1.2.3"), "{local}");
        assert!(!local.contains("::1"), "{local}");
        assert!(!local.contains("192.168.0.1"), "{local}");
        assert!(!local.contains("172.16.0.1"), "{local}");
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
            r#"<style>@import "https://tracker.example/a.css"; @import url(https://tracker.example/b.css); i { background: \75 rl(https://tracker.example/c.png); }</style>"#,
            r#"<img src="http://127.0.0.1/secret" alt="loop">"#,
            r#"<img src="http://169.254.169.254/latest" alt="meta">"#,
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
        let mut nested = String::new();
        for _ in 0..2_000 {
            nested.push_str("<div>");
        }
        nested.push_str("Still here");
        for _ in 0..2_000 {
            nested.push_str("</div>");
        }
        let deep = sanitize_html(&nested, false);
        assert!(deep.contains("Still here"), "{deep}");
        assert!(!deep.contains("&lt;div"), "{deep}");
        assert!(clean.contains(">x<") || clean.contains(">x</p>"), "{clean}");
    }

    #[test]
    fn svg_and_math_keep_their_markup() {
        let svg = sanitize_html(
            r#"<svg viewBox="0 0 10 10"><path d="M0 0"></path><script>alert(1)</script></svg>"#,
            false,
        );
        assert!(svg.contains("<svg"), "{svg}");
        assert!(svg.contains("<path"), "{svg}");
        assert!(!svg.to_ascii_lowercase().contains("<script"), "{svg}");
        let math = sanitize_html("<math><mi>x</mi></math>", false);
        assert!(math.contains("<math"), "{math}");
        assert!(math.contains("<mi"), "{math}");
        assert!(math.contains("</math>"), "{math}");
    }

    fn css_url_request(value: &str) -> Option<String> {
        let mut stripped = String::new();
        let mut rest = value;
        while let Some(start) = rest.find("/*") {
            stripped.push_str(&rest[..start]);
            let after = &rest[start + 2..];
            let end = after.find("*/")?;
            rest = &after[end + 2..];
        }
        stripped.push_str(rest);
        let open = stripped.find("url(")? + 4;
        let close = stripped[open..].find(')')? + open;
        let token = stripped[open..close]
            .trim()
            .trim_matches(|ch| ch == '"' || ch == '\'');
        (!token.is_empty()).then(|| token.to_string())
    }

    #[test]
    fn comment_in_url_is_denied_when_the_request_is_issued() {
        for css in [
            r#"url(/*x*/"http://169.254.169.254/latest/meta-data")"#,
            r#"url(/*x*/"http://127.0.0.1/x.png")"#,
        ] {
            let request = css_url_request(css).expect(css);
            assert!(
                super::request_is_blocked_with(&request, |_| Vec::new()),
                "{css} -> {request}"
            );
        }
    }

    #[test]
    fn cgnat_and_transition_addresses_are_blocked() {
        for uri in [
            "http://100.64.0.1/",
            "http://100.127.255.1/",
            "http://[::192.0.2.1]/",
            "http://[64:ff9b::192.0.2.1]/",
            "http://[2002:c000:201::]/",
            "http://[::ffff:10.1.2.3]/",
        ] {
            assert!(super::request_is_blocked_with(uri, |_| Vec::new()), "{uri}");
        }
        assert!(!super::request_is_blocked_with(
            "http://100.63.0.1/",
            |_| Vec::new()
        ));
        assert!(!super::request_is_blocked_with("http://1.1.1.1/", |_| {
            Vec::new()
        }));
    }

    #[test]
    fn redirect_to_a_private_ip_is_denied() {
        assert!(super::request_is_blocked_with(
            "http://10.0.0.5/latest",
            |_| Vec::new()
        ));
        assert!(super::request_is_blocked_with(
            "https://cdn.example/img.png",
            |_| vec![std::net::IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 5))]
        ));
        assert!(!super::request_is_blocked_with(
            "https://cdn.example/img.png",
            |_| vec![std::net::IpAddr::V4(std::net::Ipv4Addr::new(
                93, 184, 216, 34
            ))]
        ));
        assert!(!super::request_is_blocked_with(
            "about:blank",
            |_| Vec::new()
        ));
    }

    #[test]
    fn private_loopback_forms_are_rejected() {
        for url in [
            "http://127.1/",
            "http://2130706433/",
            "http://0x7f000001/",
            "http://127%2e0%2e0%2e1/",
            "http://[::ffff:7f00:1]/",
        ] {
            assert!(targets_private_network(url), "{url}");
            let clean = sanitize_html(&format!(r#"<img src="{url}" alt="x">"#), true);
            assert!(
                !clean.contains("127.1")
                    && !clean.contains("2130706433")
                    && !clean.contains("0x7f000001")
                    && !clean.contains("127%2e")
                    && !clean.contains("7f00"),
                "{url} -> {clean}"
            );
        }
    }

    #[test]
    fn style_inside_svg_cannot_smuggle_markup() {
        for html in [
            r#"<svg><style>x</STYLE><img src="https://tracker.example/p.png"></style></svg>"#,
            r#"<math><style>x</STYLE><img src="https://tracker.example/p.png"></style></math>"#,
        ] {
            let clean = sanitize_html(html, false);
            assert!(
                !clean.to_ascii_lowercase().contains("tracker.example"),
                "{html} -> {clean}"
            );
            assert!(!clean.contains("<style"), "{clean}");
            assert!(
                !clean.contains("src="),
                "remote image must not keep a url: {clean}"
            );
        }
        let broken = sanitize_html(
            "<style>a</STYLE><img src=https://tracker.example/p.png></style>",
            false,
        );
        assert!(
            !broken.to_ascii_lowercase().contains("tracker.example"),
            "{broken}"
        );
        assert!(!broken.contains("</STYLE>"), "{broken}");
        let kept = sanitize_html("<style>p { color: red }</style><p>Hi</p>", false);
        assert!(kept.contains("<style>"), "{kept}");
        assert!(kept.contains("color: red"), "{kept}");
        assert!(kept.contains("<p>Hi</p>"), "{kept}");
    }
}
