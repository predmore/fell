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
                    | "form"
                    | "input"
                    | "button"
                    | "textarea"
                    | "noscript"
                    | "head"
                    | "html"
                    | "body"
            ) {
                if name == "html" || name == "body" || name == "head" {
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
    if name_l == "href" || name_l == "src" || name_l == "srcset" {
        let lower = value.trim().to_ascii_lowercase();
        if lower.starts_with("javascript:") || lower.starts_with("vbscript:") {
            return false;
        }
    }
    if tag == "img" && (name_l == "src" || name_l == "srcset") && !allow_remote_images {
        return !is_remote_url(value);
    }
    if name_l == "style" && !allow_remote_images {
        return true;
    }
    true
}

fn rewrite_attr(tag: &str, name: &str, value: &str, allow_remote_images: bool) -> String {
    if name.eq_ignore_ascii_case("style") && !allow_remote_images {
        return strip_remote_css_urls(value);
    }
    if tag == "img" && name.eq_ignore_ascii_case("srcset") && !allow_remote_images {
        return String::new();
    }
    value.to_string()
}

fn is_remote_url(value: &str) -> bool {
    let value = value.trim().to_ascii_lowercase();
    value.starts_with("http:")
        || value.starts_with("https:")
        || value.starts_with("//")
        || value.starts_with("file:")
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
    }

    #[test]
    fn allowing_images_keeps_https_src() {
        let html = r#"<img src="https://cdn.example/a.png" alt="hero">"#;
        let clean = sanitize_html(html, true);
        assert!(clean.contains("https://cdn.example/a.png"));
    }
}
