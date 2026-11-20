use std::ops::ControlFlow;

use cssparser::{Delimiter, ParseError, Parser, ParserInput, Token};
use html5ever::namespace_url;
use scraper::ElementRef;

/// Visit the payloads of `cid:` URLs in image attributes and CSS, in document
/// order. The caller can stop the walk and decides how to match Content-IDs.
pub fn visit_cid_image_urls(
    html: &str,
    mut visit: impl FnMut(&str) -> ControlFlow<()>,
) -> ControlFlow<()> {
    let dom = crate::html::parse_html(html);
    let mut pending = vec![dom.root_element()];
    while let Some(element) = pending.pop() {
        let name = element.value().name();
        if element.value().name.ns == html5ever::ns!(html) {
            let attr = match name {
                "img" => element.attr("src"),
                "body" | "table" | "tr" | "td" | "th" => element.attr("background"),
                "video" => element.attr("poster"),
                _ => None,
            };
            if let Some(url) = attr {
                visit_cid(url, &mut visit)?;
            }
        }
        if let Some(style) = element.attr("style") {
            visit_css(style, CssRules::StyleAttribute, &mut visit)?;
        }
        if name == "style"
            && element.attr("type").is_none_or(|kind| {
                let kind = kind.trim();
                kind.is_empty() || kind.eq_ignore_ascii_case("text/css")
            })
        {
            let css = element.text().collect::<String>();
            visit_css(&css, CssRules::Stylesheet, &mut visit)?;
        }
        // Visit only element children. A template's inert contents are in a
        // document fragment, so anything inside it is skipped.
        pending.extend(element.children().rev().filter_map(ElementRef::wrap));
    }
    ControlFlow::Continue(())
}

fn visit_cid(url: &str, visit: &mut impl FnMut(&str) -> ControlFlow<()>) -> ControlFlow<()> {
    if let Some((scheme, id)) = url.trim().split_once(':')
        && scheme.eq_ignore_ascii_case("cid")
        && !id.is_empty()
    {
        visit(id)?;
    }
    ControlFlow::Continue(())
}

fn visit_css(
    css: &str,
    rules: CssRules,
    visit: &mut impl FnMut(&str) -> ControlFlow<()>,
) -> ControlFlow<()> {
    let mut input = ParserInput::new(css);
    match visit_declarations(&mut Parser::new(&mut input), rules, 0, visit) {
        Ok(()) => ControlFlow::Continue(()),
        Err(_) => ControlFlow::Break(()),
    }
}

#[derive(Clone, Copy)]
enum CssRules {
    StyleAttribute,
    Stylesheet,
    RuleBlock,
}

// Avoid an unbounded stack when walking CSS blocks and nested image functions.
const MAX_CSS_DEPTH: u32 = 64;

fn visit_declarations<'i>(
    css: &mut Parser<'i, '_>,
    rules: CssRules,
    depth: u32,
    visit: &mut impl FnMut(&str) -> ControlFlow<()>,
) -> Result<(), ParseError<'i, ()>> {
    while let Ok(token) = css.next() {
        if matches!(token, Token::Semicolon | Token::CurlyBracketBlock) {
            continue;
        }
        let allow_block = !matches!(rules, CssRules::StyleAttribute)
            && !matches!(token, Token::AtKeyword(name) if name.eq_ignore_ascii_case("font-face"));
        let declaration = !matches!(rules, CssRules::Stylesheet)
            && matches!(token, Token::Ident(_))
            && css.try_parse(Parser::expect_colon).is_ok();
        css.parse_until_before(
            Delimiter::Semicolon | Delimiter::CurlyBracketBlock,
            |value| {
                if declaration {
                    visit_css_urls(value, depth, visit)?;
                } else {
                    // Skip selectors and at-rule preludes (including @import URLs).
                    while value.next().is_ok() {}
                }
                Ok(())
            },
        )?;
        if matches!(css.next(), Ok(Token::CurlyBracketBlock))
            && allow_block
            && depth < MAX_CSS_DEPTH
        {
            css.parse_nested_block(|css| {
                visit_declarations(css, CssRules::RuleBlock, depth + 1, visit)
            })?;
        }
    }
    Ok(())
}

fn visit_css_urls<'i>(
    css: &mut Parser<'i, '_>,
    depth: u32,
    visit: &mut impl FnMut(&str) -> ControlFlow<()>,
) -> Result<(), ParseError<'i, ()>> {
    while let Ok(token) = css.next() {
        let flow = match token {
            Token::UnquotedUrl(url) => visit_cid(url, visit),
            Token::Function(name) if name.eq_ignore_ascii_case("url") => css
                .parse_nested_block(|css| Ok::<_, ParseError<'i, ()>>(css.expect_string_cloned()?))
                .map_or(ControlFlow::Continue(()), |url| visit_cid(&url, visit)),
            Token::Function(_) if depth < MAX_CSS_DEPTH => {
                css.parse_nested_block(|css| visit_css_urls(css, depth + 1, visit))?;
                continue;
            }
            _ => continue,
        };
        if flow.is_break() {
            return Err(css.new_custom_error(()));
        }
    }
    Ok(())
}
