use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::ops::{Deref, Range};

use cssparser::{Delimiter, ParseError, Parser, ParserInput, Token};

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
  img[src=""] {{
    background-color: rgba(128, 128, 128, 0.12);
    border: 0 !important;
    outline: 0 !important;
    color: transparent !important;
  }}
  img[src=""]:not([width][height]) {{ aspect-ratio: 3 / 2; }}
  img[src=""]:not([width], [height]) {{ width: 120px; }}
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
    layout: Option<Layout>,
    source_bytes: usize,
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
        let layout = Layout::from_node(*crate::html::parse_html(&body).root_element());
        Self {
            body,
            images,
            layout,
            source_bytes: html.len(),
        }
    }

    pub fn source_bytes(&self) -> usize {
        self.source_bytes
    }

    pub fn estimated_height(&self, width: f32) -> f32 {
        self.layout
            .as_ref()
            .map_or(0., |layout| layout.height(width, 20.))
            + 12.
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

#[derive(Debug)]
enum Layout {
    Text(usize),
    Image {
        width: f32,
        height: f32,
    },
    Flow {
        children: Vec<Layout>,
        scale: f32,
        padding: f32,
    },
    Row(Vec<Layout>),
}

impl Layout {
    fn from_node(node: ego_tree::NodeRef<'_, scraper::node::Node>) -> Option<Self> {
        let element = match node.value() {
            scraper::node::Node::Text(text) => {
                let chars = text.trim().chars().count();
                return (chars > 0).then_some(Self::Text(chars));
            }
            scraper::node::Node::Element(element) => element,
            _ => return None,
        };
        let name = element.name();
        let attr = |name| element.attr(name);
        if matches!(name, "head" | "script" | "style" | "title" | "svg") || attr("hidden").is_some()
        {
            return None;
        }
        if name == "img" {
            let dimension = |name| {
                let pixels = |value: &str| {
                    let value = value.trim();
                    value
                        .trim_end_matches("px")
                        .parse::<f32>()
                        .ok()
                        .filter(|px| px.is_finite() && *px > 0.)
                };
                attr("style")
                    .into_iter()
                    .flat_map(|style| style.split(';'))
                    .filter_map(|rule| rule.split_once(':'))
                    .find_map(|(property, value)| {
                        property
                            .trim()
                            .eq_ignore_ascii_case(name)
                            .then(|| pixels(value))
                            .flatten()
                    })
                    .or_else(|| attr(name).and_then(pixels))
            };
            let width = dimension("width");
            let height = dimension("height");
            return Some(Self::Image {
                width: width
                    .or_else(|| height.map(|height| height * 1.5))
                    .unwrap_or(120.),
                height: height
                    .or_else(|| width.map(|width| width / 1.5))
                    .unwrap_or(80.),
            });
        }
        if matches!(name, "br" | "hr") {
            return Some(Self::Flow {
                children: Vec::new(),
                scale: 1.,
                padding: 20.,
            });
        }
        let mut children: Vec<Layout> = Vec::new();
        for child in node.children().filter_map(Self::from_node) {
            if let (Some(Self::Text(before)), Self::Text(after)) = (children.last_mut(), &child) {
                *before += 1 + after;
            } else {
                children.push(child);
            }
        }
        if children.is_empty() {
            return None;
        }
        let (scale, padding) = match name {
            "tr" => return Some(Self::Row(children)),
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => (1.9, 18.),
            "td" | "th" => (1., 12.),
            "p" | "ul" | "ol" | "blockquote" => (1., 12.),
            "a" | "b" | "em" | "font" | "i" | "small" | "span" | "strong" | "u"
                if children.len() == 1 =>
            {
                return children.pop();
            }
            _ => (1., 0.),
        };
        Some(Self::Flow {
            children,
            scale,
            padding,
        })
    }

    fn height(&self, width: f32, line: f32) -> f32 {
        match self {
            Self::Text(chars) => (*chars as f32 * line * 0.44 / width.max(1.)).ceil() * line,
            Self::Image {
                width: natural,
                height,
            } => height * (width / natural).min(1.),
            Self::Flow {
                children,
                scale,
                padding,
            } => {
                children
                    .iter()
                    .map(|node| node.height(width, line * scale))
                    .sum::<f32>()
                    + padding
            }
            Self::Row(cells) => cells
                .iter()
                .map(|node| node.height((width / cells.len() as f32 - 12.).max(1.), line))
                .fold(0., f32::max),
        }
    }
}

#[cfg(test)]
#[test]
fn height_estimates_account_for_headings_image_hints_and_table_rows() {
    let mail = WebMail::prepare(
        r#"<table><tr><td><h1>Today</h1></td><td><h1>News</h1></td></tr><tr><td colspan="2"><img width="300" src="https://example.test/photo"></td></tr></table>"#,
    );
    let height = mail.estimated_height(420.);
    assert!((260. ..300.).contains(&height), "{height}");
    assert!(mail.estimated_height(180.) < height);
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
                    serialize_css(&css, out, Some(slots), escape_style);
                } else {
                    serialize_css(&css, out, None, escape_style);
                }
                out.push_str("</style>");
                return;
            }
            out.push('<');
            out.push_str(name);
            for (name_attr, value) in &element.attrs {
                let attr = name_attr.local.as_ref();
                // Only image attributes and image-valued CSS declarations
                // may enter the fetch list.
                if let Some(slots) = images.as_deref_mut() {
                    if name_attr.prefix.is_none()
                        && (attr == "src" && name.eq_ignore_ascii_case("img")
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
                        let start = out.len();
                        let first_slot = slots.len();
                        out.push_str(" style=\"");
                        serialize_css(value, out, Some(slots), escape_attr);
                        out.push('"');
                        if contains_external_url(value) && contains_external_url(&out[start..]) {
                            out.truncate(start);
                            slots.truncate(first_slot);
                        }
                        continue;
                    }
                    let link = attr == "href"
                        && name_attr.prefix.is_none()
                        && (name.eq_ignore_ascii_case("a") || name.eq_ignore_ascii_case("area"));
                    if !link && !keep_offline_attr(element, attr, value) {
                        continue;
                    }
                }
                if !keep_attr(name, attr, value, allow_remote_images) {
                    continue;
                }
                let value = rewrite_attr(name, attr, value, allow_remote_images);
                out.push(' ');
                if let Some(prefix) = &name_attr.prefix {
                    out.push_str(prefix.as_ref());
                    out.push(':');
                }
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

fn resource_attr(name: &str) -> bool {
    matches!(
        name,
        "href"
            | "xlink:href"
            | "src"
            | "srcset"
            | "imagesrcset"
            | "poster"
            | "background"
            | "lowsrc"
            | "dynsrc"
            | "data"
            | "codebase"
            | "archive"
            | "classid"
            | "manifest"
            | "profile"
            | "longdesc"
            | "usemap"
            | "action"
            | "formaction"
            | "cite"
            | "base"
            | "xml:base"
    )
}

fn local_resource(value: &str) -> bool {
    let value = value.trim();
    let (scheme, _) = value.split_once(':').unwrap_or_default();
    value.is_empty()
        || value.starts_with('#')
        || scheme.eq_ignore_ascii_case("cid")
        || value
            .get(..11)
            .is_some_and(|head| head.eq_ignore_ascii_case("data:image/"))
}

fn contains_external_url(value: &str) -> bool {
    // Base64 may contain consecutive slashes without containing a URL.
    if let Some((header, payload)) = value.split_once(',')
        && header.to_ascii_lowercase().starts_with("data:image/")
        && header.to_ascii_lowercase().ends_with(";base64")
        && payload
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'='))
    {
        return false;
    }
    let normalized = normalize_resource_url(value).to_ascii_lowercase();
    ["http:", "https:", "//", "file:"]
        .iter()
        .any(|scheme| normalized.contains(scheme))
}

fn keep_offline_attr(element: &scraper::node::Element, name: &str, value: &str) -> bool {
    if matches!(name, "ping" | "attributionsrc" | "base") || contains_external_url(value) {
        return false;
    }
    if matches!(name, "srcset" | "imagesrcset") {
        // A comma can separate candidates or belong to a data URL. Keep
        // only single local resource attributes and omit source sets.
        return false;
    }
    if resource_attr(name) {
        return local_resource(value);
    }
    if matches!(name, "values" | "from" | "to" | "by")
        && [
            "set",
            "animate",
            "animateColor",
            "animateTransform",
            "animateMotion",
        ]
        .iter()
        .any(|tag| tag.eq_ignore_ascii_case(element.name()))
        && element.attr("attributeName").is_some_and(|target| {
            let target = target.to_ascii_lowercase();
            resource_attr(&target) || target == "style"
        })
    {
        return value.split(';').all(local_resource);
    }
    if value.contains('(') {
        let css = strip_css_comments(value);
        let mut input = ParserInput::new(&css);
        let mut resources = Vec::new();
        css_resources(&mut Parser::new(&mut input), 0, false, &mut resources);
        return resources.into_iter().all(|(_, resource)| match resource {
            CssResource::Url(url) | CssResource::ImageString(url) => local_resource(&url),
            _ => false,
        });
    }
    true
}

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
        return strip_remote_css_urls(value);
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
        "href" | "xlink:href" if image_href_tag(tag) => is_remote_url(value),
        _ => false,
    }
}

fn image_href_tag(tag: &str) -> bool {
    ["img", "image", "use", "feimage"]
        .iter()
        .any(|name| tag.eq_ignore_ascii_case(name))
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
    let mut out = String::new();
    serialize_css(css, &mut out, None, str::to_owned);
    out
}

fn strip_css_comments(css: &str) -> String {
    let mut out = String::with_capacity(css.len());
    let mut chars = css.chars().peekable();
    let mut quote = None;
    while let Some(ch) = chars.next() {
        match (quote, ch) {
            (_, '\\') => {
                out.push(ch);
                out.extend(chars.next());
                continue;
            }
            (None, '"' | '\'') => quote = Some(ch),
            (Some(open), ch) if ch == open => quote = None,
            (None, '/') if chars.next_if_eq(&'*').is_some() => {
                while let Some(ch) = chars.next() {
                    if ch == '*' && chars.next_if_eq(&'/').is_some() {
                        break;
                    }
                }
                continue;
            }
            _ => {}
        }
        out.push(ch);
    }
    out
}

enum CssResource {
    Remove,
    Url(String),
    ImageString(String),
    VariableCandidate,
}

fn css_resources(
    css: &mut Parser<'_, '_>,
    depth: usize,
    image_strings: bool,
    resources: &mut Vec<(Range<usize>, CssResource)>,
) {
    loop {
        let start = css.position().byte_index();
        let resource = match css.next_including_whitespace_and_comments() {
            Ok(Token::AtKeyword(name)) if name.eq_ignore_ascii_case("import") => {
                let _ = css.parse_until_after(Delimiter::Semicolon, |rule| {
                    while rule.next().is_ok() {}
                    Ok::<_, ParseError<'_, ()>>(())
                });
                CssResource::Remove
            }
            Ok(Token::UnquotedUrl(url)) => CssResource::Url(url.to_string()),
            Ok(Token::Function(name)) if name.eq_ignore_ascii_case("url") => css
                .parse_nested_block(|args| {
                    Ok::<_, ParseError<'_, ()>>(args.expect_string()?.to_string())
                })
                .map_or(CssResource::Remove, CssResource::Url),
            Ok(Token::QuotedString(value)) if image_strings => {
                CssResource::ImageString(value.to_string())
            }
            // A standalone URL string can also become an image source via var().
            Ok(Token::QuotedString(value)) if contains_external_url(value) => {
                CssResource::VariableCandidate
            }
            Ok(Token::BadString(_)) if image_strings => CssResource::Remove,
            Ok(Token::BadUrl(_)) => CssResource::Remove,
            Ok(
                token @ (Token::Function(_)
                | Token::ParenthesisBlock
                | Token::SquareBracketBlock
                | Token::CurlyBracketBlock),
            ) => {
                let nested_images = match token {
                    Token::Function(name) => {
                        [
                            "image-set",
                            "-webkit-image-set",
                            "image",
                            "cross-fade",
                            "-webkit-cross-fade",
                        ]
                        .iter()
                        .any(|function| name.eq_ignore_ascii_case(function))
                            || image_strings && name.eq_ignore_ascii_case("var")
                    }
                    _ => false,
                };
                let _ = css.parse_nested_block(|nested| {
                    if depth < 64 {
                        css_resources(nested, depth + 1, nested_images, resources);
                    }
                    Ok::<_, ParseError<'_, ()>>(())
                });
                if depth < 64 {
                    continue;
                }
                CssResource::Remove
            }
            Ok(_) => continue,
            Err(_) => break,
        };
        resources.push((start..css.position().byte_index(), resource));
    }
}

fn css_declaration(css: &str, start: usize) -> Option<&str> {
    css[..start]
        .rsplit([';', '{', '}'])
        .next()?
        .split_once(':')
        .map(|(name, _)| name.trim())
}

fn serialize_css(
    css: &str,
    out: &mut String,
    mut images: Option<&mut Vec<(usize, String)>>,
    escape: impl Fn(&str) -> String,
) {
    // Classify comment-split spellings before tokenizing. The tokenizer also
    // handles CSS escapes and keeps semicolons and parentheses inside strings.
    let css = strip_css_comments(css);
    let mut input = ParserInput::new(&css);
    let mut resources = Vec::new();
    css_resources(&mut Parser::new(&mut input), 0, false, &mut resources);
    let mut from = 0;
    for (range, resource) in resources {
        let name = css_declaration(&css, range.start);
        if images.is_none()
            && matches!(resource, CssResource::VariableCandidate)
            && !name.is_some_and(|name| name.starts_with("--"))
        {
            continue;
        }
        if let CssResource::Url(value) | CssResource::ImageString(value) = &resource
            && (value.is_empty()
                || normalize_resource_url(value)
                    .to_ascii_lowercase()
                    .starts_with("data:image/"))
        {
            continue;
        }
        out.push_str(&escape(&css[from..range.start]));
        let quoted = matches!(resource, CssResource::ImageString(_));
        if let CssResource::Url(value) | CssResource::ImageString(value) = resource {
            let slot = images.as_deref_mut().filter(|slots| slots.len() < 48);
            if let Some(slots) = slot
                && name.is_some_and(|name| {
                    (quoted && name.starts_with("--"))
                        || matches!(
                            name.to_ascii_lowercase().as_str(),
                            "background" | "background-image" | "border-image" | "list-style-image"
                        )
                })
                && let Some(url) = remote_image_url(&value)
            {
                out.push_str(&escape(if quoted { "\"" } else { "url(\"" }));
                slots.push((out.len(), url));
                out.push_str(&escape(if quoted { "\"" } else { "\")" }));
            } else if quoted {
                out.push_str(&escape("\"\""));
            } else {
                out.push_str(" none ");
            }
        } else {
            out.push(' ');
        }
        from = range.end;
    }
    out.push_str(&escape(&css[from..]));
}

fn image_attr_is_blocked(tag: &str, name: &str, value: &str, allow_remote_images: bool) -> bool {
    match name {
        "src" | "poster" | "background" | "lowsrc" | "dynsrc" => {
            resource_is_disallowed(value, allow_remote_images)
        }
        "srcset" => value
            .split(',')
            .any(|part| resource_is_disallowed(part, allow_remote_images)),
        "href" | "xlink:href" if image_href_tag(tag) => {
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
    // IANA special-purpose address blocks, plus multicast. Sub-allocations
    // with global reachability still have protocol-specific purposes.
    matches!(
        ip.octets(),
        [0 | 10 | 127 | 224..=255, ..]
            | [100, 64..=127, ..]
            | [169, 254, ..]
            | [172, 16..=31, ..]
            | [192, 0, 0 | 2, _]
            | [192, 31, 196, _]
            | [192, 52, 193, _]
            | [192, 88, 99, _]
            | [192, 168, ..]
            | [192, 175, 48, _]
            | [198, 18 | 19, ..]
            | [198, 51, 100, _]
            | [203, 0, 113, _]
    )
}

fn ipv6_is_blocked(ip: Ipv6Addr) -> bool {
    // Require global unicast. This rules out mapped and compatible IPv4,
    // translation prefixes, loopback, ULA, link-local and multicast.
    !matches!(ip.segments()[0], 0x2000..=0x3fff)
        || matches!(
            ip.segments(),
            [0x2001, 0x0000..=0x01ff | 0x0db8, ..]
                | [0x2002, ..]
                | [0x2620, 0x004f, 0x8000, ..]
                | [0x3fff, 0x0000..=0x0fff, ..]
        )
}

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
        return ipv4_is_blocked(Ipv4Addr::from(octets));
    }
    if host.contains(':')
        && let Some(groups) = parse_ipv6(&host)
    {
        return ipv6_is_blocked(Ipv6Addr::from(groups));
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
    fn quoted_css_images_are_empty_until_consented() {
        for declaration in [
            r#"background: image-set("https://cdn.example.test/a" 1x)"#,
            r#"background: -webkit-image-set(url("") 1x, "https://cdn.example.test/a" 2x)"#,
            r#"background: image("https://cdn.example.test/a")"#,
            r#"background: cross-fade("https://cdn.example.test/a", red)"#,
            r#"--y: image-set("https://cdn.example.test/a" 1x); background: var(--y)"#,
            r#"background: IMAGE-SET(var(--y, "https://cdn.example.test/a") 1x)"#,
        ] {
            for html in [
                format!("<style>p {{ {declaration} }}</style><p>Body</p>"),
                format!("<p style='{declaration}'>Body</p>"),
            ] {
                let mail = WebMail::prepare(&html);
                let blocked = mail.page("light", |_| None);
                assert!(!blocked.contains("https://"), "{blocked}");
                assert!(mail.has_remote_images(), "{html}");
                let allowed = mail.page("light", |url| {
                    assert_eq!(url, "https://cdn.example.test/a");
                    Some("data:image/png;base64,AAAA".into())
                });
                assert!(allowed.contains("data:image/png;base64,AAAA"), "{allowed}");
            }
        }
        let page = WebMail::prepare(
            r#"<style>p {--y: "https://cdn.example.test/a"; background: image-set(var(--y) 1x, "data:image/png;base64,AAAA" type("image/png") 2x);}</style>"#,
        )
        .page("light", |_| None);
        assert!(!page.contains("https://"), "{page}");
        assert!(page.contains(r#""data:image/png;base64,AAAA" type("image/png")"#));
    }

    #[test]
    fn blocked_images_keep_layout_hints() {
        let page = WebMail::prepare(
            r#"<img src="https://cdn.example.test/hero" width="420" height="240" style="max-width: 90%"><img src="https://cdn.example.test/card" width="240">"#,
        )
        .page("light", |_| None);
        let dom = crate::html::parse_html(&page);
        let images: Vec<_> = dom
            .select(&scraper::Selector::parse("img").unwrap())
            .collect();
        assert_eq!(images.len(), 2);
        assert_eq!(images[0].attr("src"), Some(""));
        assert_eq!(images[0].attr("width"), Some("420"));
        assert_eq!(images[0].attr("height"), Some("240"));
        assert_eq!(images[0].attr("style"), Some("max-width: 90%"));
        assert_eq!(images[1].attr("src"), Some(""));
        assert_eq!(images[1].attr("width"), Some("240"));
        assert!(page.contains("aspect-ratio: 3 / 2"));
        assert!(!page.contains("content: url"));
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

    #[test]
    fn offline_page_omits_external_svg_and_attribute_resources() {
        let cases = [
            r#"<svg><feImage href="https://probe.invalid/image"/></svg>"#,
            r#"<svg><feImage xlink:href="//probe.invalid/xlink"/></svg>"#,
            r#"<svg><textPath href="HTTP://probe.invalid/path">Text</textPath></svg>"#,
            r#"<svg><linearGradient href="https://probe.invalid/gradient"/></svg>"#,
            r#"<svg><pattern href="file:///probe.png"/></svg>"#,
            r#"<svg><filter href="relative-probe.svg#filter"/></svg>"#,
            r#"<svg><mpath href="https://probe.invalid/motion"/></svg>"#,
            r#"<svg><cursor href="//probe.invalid/cursor"/></svg>"#,
            r#"<svg><font-face-uri href="https://probe.invalid/font"/></svg>"#,
            r#"<svg><path fill="url(https://probe.invalid/fill)"/></svg>"#,
            r#"<svg><path stroke="url(file:///probe.png)"/></svg>"#,
            r#"<svg><path filter="url(//probe.invalid/filter)"/></svg>"#,
            r#"<svg><path mask="url('relative-probe.svg#mask')"/></svg>"#,
            r#"<svg><path clip-path="url(ht&#10;tps://probe.invalid/clip)"/></svg>"#,
            r#"<svg><set attributeName="href" to="relative-probe.svg"/></svg>"#,
            r#"<svg><set to="url(file:///probe.png)"/></svg>"#,
            r##"<svg><animate values="#local;https://probe.invalid/next"/></svg>"##,
            r#"<svg><animate from="//probe.invalid/from"/></svg>"#,
            r#"<svg><animate to="https://probe.invalid/to"/></svg>"#,
            r#"<svg><animate by="url(relative-probe.svg)"/></svg>"#,
            r#"<img srcset="relative-probe.png 2x" src="relative-probe.png">"#,
            r#"<p data-src="photo: //probe.invalid/data" title="see file:///probe.png">Text</p>"#,
            r#"<p style='font-family: "https://probe.invalid/family"'>Text</p>"#,
            r#"<svg><a xlink:href="https://probe.invalid/link">Text</a></svg>"#,
        ];
        for html in cases {
            let mail = WebMail::prepare(html);
            assert!(!mail.has_remote_images(), "{html}");
            let page = mail.page("light", |_| None);
            for forbidden in ["probe.invalid", "file:", "relative-probe"] {
                assert!(!page.contains(forbidden), "{html}: {page}");
            }
        }
        let page = WebMail::prepare(
            r##"<svg><use href="#local" fill="url(#accent)"/></svg><a href="https://example.test">Link</a><img src="cid:logo"><img src="data:image/png;base64,//AA">"##,
        )
        .page("light", |_| None);
        for local in [
            "#local",
            "url(#accent)",
            "https://example.test",
            "cid:logo",
            "//AA",
        ] {
            assert!(page.contains(local), "{page}");
        }
    }

    #[test]
    fn sanitized_page_removes_comment_split_imports_and_unapproved_urls() {
        let mail = WebMail::prepare(
            r#"<style>
            @im/*x*/port "https://example.test/import;with-semicolon";
            @import url("data:text/css,body{}");
            @font-face { src: u/*x*/rl("http://example.test/font"); }
            p { background: url/*x*/("https://example.test/slot(a).png"); }
            i { background: url(/*x*/"http://192.0.0.192/metadata"); }
            b { background: url("file:///probe.png"); }
            em { background: url("data:image/png;base64,AAAA"); }
            </style><p style="mask: u/*x*/rl(https://example.test/mask)">Sample</p>"#,
        );
        let blocked = mail.page("light", |_| None);
        let mut slots = Vec::new();
        let allowed = mail.page("light", |url| {
            slots.push(url.to_owned());
            Some("data:image/png;base64,BBBB".into())
        });
        for page in [&blocked, &allowed] {
            assert!(!page.contains("example.test"), "{page}");
            assert!(!page.contains("192.0.0.192"), "{page}");
            assert!(!page.contains("file:"), "{page}");
            assert!(!page.contains("@import"), "{page}");
            assert!(!page.contains("/*x*/"), "{page}");
            assert!(page.contains("data:image/png;base64,AAAA"), "{page}");
        }
        assert_eq!(slots, ["https://example.test/slot(a).png"]);
        assert!(allowed.contains("data:image/png;base64,BBBB"));
    }

    #[test]
    fn special_purpose_addresses_are_blocked_before_connect() {
        for ip in [
            "192.0.0.192",
            "192.0.2.1",
            "192.31.196.1",
            "192.52.193.1",
            "192.88.99.2",
            "192.175.48.1",
            "198.19.255.255",
            "198.51.100.1",
            "203.0.113.1",
            "224.0.0.1",
            "240.0.0.1",
            "255.255.255.255",
            "::ffff:1.1.1.1",
            "64:ff9b:1::1",
            "100::1",
            "2001:2::1",
            "2001:db8::1",
            "2620:4f:8000::1",
            "3fff:fff::1",
            "5f00::1",
            "fc00::1",
            "fe80::1",
            "ff02::1",
        ] {
            let address = ip.parse().unwrap();
            assert!(address_is_blocked(address), "{ip}");
            assert!(
                request_is_blocked_with("https://cdn.example/image", |_| vec![address]),
                "{ip}"
            );
        }
        for ip in ["192.0.1.1", "223.255.255.254", "2606:4700:4700::1111"] {
            assert!(!address_is_blocked(ip.parse().unwrap()), "{ip}");
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
