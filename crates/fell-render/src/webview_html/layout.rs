//! Approximate dimensions from the sanitized mail's box tree and cascade.

use std::collections::HashMap;

use cssparser::{
    AtRuleParser, ParseError, Parser, ParserInput, ParserState, QualifiedRuleParser,
    StyleSheetParser, Token,
};
use scraper::node::{Element, Node};
use selectors::matching::{
    MatchingContext, MatchingForInvalidation, MatchingMode, NeedsSelectorFlags, QuirksMode,
    matches_selector,
};

use crate::html::{self, Declaration};

#[cfg(test)]
use super::WebMail;

const MAX_ESTIMATED_HEIGHT: f32 = 8_192.;
// Allow for font metrics and line rounding across WebKit installations.
const HEIGHT_ALLOWANCE: f32 = 12.;

#[derive(Debug)]
pub(super) struct Estimate(Vec<(i32, Option<Layout>)>);

impl Estimate {
    pub(super) fn prepare(layout_dom: &scraper::Html, metrics: TextMetrics) -> Self {
        let styles = LayoutStyles::from_document(layout_dom);
        let root = *layout_dom.root_element();
        let font = Font {
            size: 16.,
            line: LineHeight::Normal,
            metrics,
        };
        let layouts = if styles.breakpoints.len() <= 8 {
            std::iter::once(1)
                .chain(styles.breakpoints.iter().copied())
                .map(|width| {
                    (
                        width,
                        Layout::from_node(root, &styles, Some(width), font, false),
                    )
                })
                .collect()
        } else {
            vec![(1, Layout::from_node(root, &styles, None, font, false))]
        };
        Self(layouts)
    }

    pub(super) fn height(&self, width: f32, image_limit: f32) -> f32 {
        let index = self
            .0
            .partition_point(|(min, _)| *min <= width.round() as i32);
        (self.0[index.saturating_sub(1)]
            .1
            .as_ref()
            .map_or(0., |layout| {
                layout.measure(width, image_limit).outer_height()
            })
            + HEIGHT_ALLOWANCE)
            .min(MAX_ESTIMATED_HEIGHT)
    }
}

// Index by one atom of each compound selector so a large ESP stylesheet
// is cheap to match against each element.
#[derive(Clone, PartialEq, Eq, Hash)]
enum SelectorAtom {
    Tag(String),
    Class(String),
    Id(String),
}

enum Selector {
    Compound(Vec<SelectorAtom>),
    Complex(Result<selectors::parser::Selector<scraper::selector::Simple>, Vec<SelectorAtom>>),
    AtRestInactive,
}

fn inactive_pseudo(name: &str) -> bool {
    [
        "active",
        "hover",
        "focus",
        "focus-visible",
        "focus-within",
        "visited",
    ]
    .iter()
    .any(|state| name.eq_ignore_ascii_case(state))
}

fn at_rest_selector(input: &mut Parser<'_, '_>, depth: usize) -> String {
    let mut source = String::new();
    let mut unchanged = input.position();
    loop {
        let start = input.position();
        let Ok(token) = input.next_including_whitespace().cloned() else {
            break;
        };
        match token {
            Token::Colon => {
                if input.try_parse(Parser::expect_colon).is_ok() {
                    continue;
                }
                if input
                    .try_parse(|input| input.expect_ident_cloned())
                    .is_ok_and(|name| inactive_pseudo(&name))
                {
                    source.push_str(input.slice(unchanged..start));
                    // An impossible child index has class specificity and
                    // composes with :not(), :is() and :where().
                    source.push_str(":nth-child(0)");
                    unchanged = input.position();
                }
            }
            Token::Function(name)
                if depth < 32
                    && ["not", "is", "where", "has"]
                        .iter()
                        .any(|pseudo| name.eq_ignore_ascii_case(pseudo)) =>
            {
                let arguments = input.position();
                if let Ok(rest) = input.parse_nested_block(|input| {
                    Ok::<_, ParseError<'_, ()>>(at_rest_selector(input, depth + 1))
                }) {
                    source.push_str(input.slice(unchanged..arguments));
                    source.push_str(&rest);
                    source.push(')');
                    unchanged = input.position();
                }
            }
            _ => {}
        }
    }
    source.push_str(input.slice_from(unchanged));
    source
}

impl Selector {
    fn parse(input: &mut Parser<'_, '_>) -> Self {
        let start = input.position();
        let mut atoms = Vec::new();
        let mut compound = true;
        let mut universal = false;
        let mut separated = false;
        while let Ok(token) = input.next_including_whitespace().cloned() {
            match token {
                Token::WhiteSpace(_) => separated = !atoms.is_empty(),
                Token::Delim('*') if atoms.is_empty() && !universal => universal = true,
                Token::Ident(tag) if (atoms.is_empty() && !universal) || separated => {
                    if separated {
                        atoms.clear();
                        compound = false;
                    }
                    atoms.push(SelectorAtom::Tag(tag.to_ascii_lowercase()));
                    separated = false;
                }
                Token::Delim('.') | Token::IDHash(_) => {
                    if separated {
                        atoms.clear();
                        compound = false;
                        separated = false;
                    }
                    match token {
                        Token::IDHash(id) => atoms.push(SelectorAtom::Id(id.to_string())),
                        _ if let Ok(Token::Ident(class)) = input.next_including_whitespace() => {
                            atoms.push(SelectorAtom::Class(class.to_string()));
                        }
                        _ => compound = false,
                    }
                }
                Token::Delim('>') | Token::Delim('+') | Token::Delim('~') | Token::Delim('|') => {
                    atoms.clear();
                    separated = false;
                    compound = false;
                }
                Token::Colon => {
                    let state = input.try_parse(|input| {
                        let pseudo_element = input.try_parse(Parser::expect_colon).is_ok();
                        let name = input.expect_ident()?;
                        Ok::<_, cssparser::BasicParseError<'_>>(if pseudo_element {
                            name.eq_ignore_ascii_case("selection")
                        } else {
                            inactive_pseudo(name)
                        })
                    });
                    if state == Ok(true) {
                        return Self::AtRestInactive;
                    }
                    if separated {
                        atoms.clear();
                    }
                    separated = false;
                    compound = false;
                }
                _ => {
                    if separated {
                        atoms.clear();
                    }
                    separated = false;
                    compound = false;
                }
            }
        }
        if compound && (universal || !atoms.is_empty()) {
            Self::Compound(atoms)
        } else {
            let mut source = ParserInput::new(input.slice_from(start).trim());
            let source = at_rest_selector(&mut Parser::new(&mut source), 0);
            let mut source = ParserInput::new(&source);
            let mut source = Parser::new(&mut source);
            Self::Complex(
                source
                    .parse_entirely(|input| {
                        selectors::parser::Selector::parse(&scraper::selector::Parser, input)
                    })
                    .map_err(|_| atoms),
            )
        }
    }

    fn key(&self) -> Option<&SelectorAtom> {
        let atoms = match self {
            Self::Compound(atoms) | Self::Complex(Err(atoms)) => atoms,
            _ => return None,
        };
        atoms.iter().max_by_key(|atom| atom.specificity())
    }

    fn matches(&self, node: scraper::ElementRef<'_>) -> bool {
        match self {
            Self::AtRestInactive => false,
            Self::Complex(Ok(selector)) => matches_selector(
                selector,
                0,
                None,
                &node,
                &mut MatchingContext::new(
                    MatchingMode::Normal,
                    None,
                    &mut Default::default(),
                    QuirksMode::NoQuirks,
                    NeedsSelectorFlags::No,
                    MatchingForInvalidation::No,
                ),
            ),
            Self::Compound(atoms) | Self::Complex(Err(atoms)) => {
                atoms.iter().all(|atom| atom.matches(node.value()))
            }
        }
    }

    fn specificity(&self) -> u32 {
        let atoms = match self {
            Self::Compound(atoms) => atoms,
            Self::Complex(Ok(selector)) => return selector.specificity(),
            _ => return 0,
        };
        let mut counts = [0_u32; 3];
        for atom in atoms {
            counts[atom.specificity()] += 1;
        }
        // Servo packs the ID, class and type counts into 10 bits each.
        let [types, classes, ids] = counts.map(|count| count.min(1023));
        (ids << 20) | (classes << 10) | types
    }
}

impl SelectorAtom {
    fn specificity(&self) -> usize {
        match self {
            Self::Tag(_) => 0,
            Self::Class(_) => 1,
            Self::Id(_) => 2,
        }
    }

    fn matches(&self, element: &Element) -> bool {
        match self {
            Self::Tag(tag) => element.name() == tag,
            Self::Class(class) => element.has_class(class, scraper::CaseSensitivity::CaseSensitive),
            Self::Id(id) => element.id() == Some(id),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Applicability {
    No,
    Uncertain,
    Yes,
}

#[derive(Clone, Debug)]
struct MediaQuery {
    min: i32,
    max: i32,
    unknown: bool,
}

type Media = Vec<MediaQuery>;

impl MediaQuery {
    fn parse(input: &mut Parser<'_, '_>) -> Self {
        let mut query = Self {
            min: i32::MIN,
            max: i32::MAX,
            unknown: false,
        };
        if let Ok(mut medium) = input.try_parse(Parser::expect_ident_cloned) {
            if medium.eq_ignore_ascii_case("only")
                && let Ok(qualified) = input.expect_ident_cloned()
            {
                medium = qualified;
            }
            if medium.eq_ignore_ascii_case("print") {
                query.max = -1;
                return query;
            }
            query.unknown =
                !(medium.eq_ignore_ascii_case("all") || medium.eq_ignore_ascii_case("screen"));
            if input.is_exhausted() {
                return query;
            }
            if input.expect_ident_matching("and").is_err() {
                query.unknown = true;
                return query;
            }
        }
        loop {
            let condition = input.try_parse(|input| {
                input.expect_parenthesis_block()?;
                input.parse_nested_block(|input| {
                    let feature = input.expect_ident_cloned()?;
                    input.expect_colon()?;
                    let start = input.position();
                    input.next()?;
                    let width = css_pixels(input.slice_from(start).trim())
                        .ok_or_else(|| input.new_custom_error::<_, ()>(()))?;
                    input.expect_exhausted()?;
                    Ok((feature, width))
                })
            });
            match condition {
                Ok((feature, width)) => match feature.to_ascii_lowercase().as_str() {
                    "min-width" => query.min = query.min.max(width.ceil() as i32),
                    "max-width" => query.max = query.max.min(width.floor() as i32),
                    "width" => {
                        query.min = query.min.max(width.ceil() as i32);
                        query.max = query.max.min(width.floor() as i32);
                    }
                    _ => query.unknown = true,
                },
                Err(_) => {
                    query.unknown = true;
                    break;
                }
            }
            if input.is_exhausted() {
                break;
            }
            if input.expect_ident_matching("and").is_err() {
                query.unknown = true;
                break;
            }
        }
        query
    }

    fn applies(&self, width: Option<i32>) -> Applicability {
        if self.max < self.min
            || self.max < 1
            || width.is_some_and(|width| !(self.min..=self.max).contains(&width))
        {
            Applicability::No
        } else if self.unknown || width.is_none() && (self.min > i32::MIN || self.max < i32::MAX) {
            Applicability::Uncertain
        } else {
            Applicability::Yes
        }
    }
}

fn media_queries(input: &mut Parser<'_, '_>) -> Media {
    input
        .parse_comma_separated(|input| {
            let query = MediaQuery::parse(input);
            // Unknown conditions may still apply. Consume the rest of this query.
            while input.next().is_ok() {}
            Ok::<_, ParseError<'_, ()>>(query)
        })
        .unwrap_or_else(|_| {
            vec![MediaQuery {
                min: i32::MIN,
                max: i32::MAX,
                unknown: true,
            }]
        })
}

fn style_media(element: &Element) -> Option<Media> {
    element.attr("media").map(|value| {
        let mut input = ParserInput::new(value);
        media_queries(&mut Parser::new(&mut input))
    })
}

pub(super) fn style_applies(element: &Element) -> bool {
    style_media(element).is_none_or(|media| {
        media
            .iter()
            .any(|query| query.applies(None) != Applicability::No)
    })
}

struct StyleRule {
    selector: Selector,
    declarations: Vec<Declaration>,
    media: Vec<Media>,
    uncertain: bool,
    conditional: bool,
    has_layout: bool,
    order: usize,
}

#[derive(Default)]
pub(crate) struct LayoutStyles {
    indexed: HashMap<SelectorAtom, Vec<StyleRule>>,
    unindexed: Vec<StyleRule>,
    incomplete: bool,
    breakpoints: Vec<i32>,
    order: usize,
}

struct LayoutRules<'a> {
    styles: &'a mut LayoutStyles,
    media: Vec<Media>,
    uncertain: bool,
    depth: usize,
}

impl<'i> AtRuleParser<'i> for LayoutRules<'_> {
    type Prelude = Option<Media>;
    type AtRule = ();
    type Error = ();

    fn parse_prelude<'t>(
        &mut self,
        name: cssparser::CowRcStr<'i>,
        input: &mut Parser<'i, 't>,
    ) -> Result<Self::Prelude, ParseError<'i, ()>> {
        if name.eq_ignore_ascii_case("media") {
            return Ok(Some(media_queries(input)));
        }
        // Fonts and animations have declaration bodies, not selector lists.
        if matches!(
            name.to_ascii_lowercase().as_str(),
            "font-face" | "keyframes" | "-webkit-keyframes" | "page"
        ) {
            return Err(input.new_custom_error(()));
        }
        while input.next().is_ok() {}
        Ok(None)
    }

    fn rule_without_block(&mut self, _: Self::Prelude, _: &ParserState) -> Result<(), ()> {
        Ok(())
    }

    fn parse_block<'t>(
        &mut self,
        media: Self::Prelude,
        _: &ParserState,
        input: &mut Parser<'i, 't>,
    ) -> Result<(), ParseError<'i, ()>> {
        if self.depth == 16 {
            self.styles.incomplete = true;
            return Ok(());
        }
        let uncertain = self.uncertain;
        let guarded = media.is_some();
        self.uncertain |= !guarded;
        if let Some(media) = media {
            self.push_media(media);
        }
        self.depth += 1;
        self.parse(input);
        self.depth -= 1;
        if guarded {
            self.media.pop();
        }
        self.uncertain = uncertain;
        Ok(())
    }
}

impl<'i> QualifiedRuleParser<'i> for LayoutRules<'_> {
    type Prelude = Vec<Selector>;
    type QualifiedRule = ();
    type Error = ();

    fn parse_prelude<'t>(
        &mut self,
        input: &mut Parser<'i, 't>,
    ) -> Result<Self::Prelude, ParseError<'i, ()>> {
        input.parse_comma_separated(|input| {
            let selector = Selector::parse(input);
            while input.next().is_ok() {}
            Ok(selector)
        })
    }

    fn parse_block<'t>(
        &mut self,
        selectors: Self::Prelude,
        _: &ParserState,
        input: &mut Parser<'i, 't>,
    ) -> Result<(), ParseError<'i, ()>> {
        let declarations = html::parse_stylesheet_declarations(input);
        let uncertain = self.uncertain
            || declarations
                .as_ref()
                .map_or(true, |rules| rules.iter().any(|rule| !can_model(rule)));
        let has_layout = declarations
            .as_ref()
            .map_or(true, |rules| rules.iter().any(|rule| !neutral(rule)));
        let declarations = declarations.unwrap_or_default();
        let invalid_selector = selectors
            .iter()
            .any(|selector| matches!(selector, Selector::Complex(Err(_))));
        for selector in selectors {
            if matches!(selector, Selector::AtRestInactive) {
                continue;
            }
            let key = selector.key().cloned();
            let rule = StyleRule {
                selector,
                declarations: declarations.clone(),
                media: self.media.clone(),
                uncertain,
                conditional: self.uncertain || invalid_selector,
                has_layout,
                order: self.styles.order,
            };
            match key {
                Some(key) => self.styles.indexed.entry(key).or_default().push(rule),
                None => self.styles.unindexed.push(rule),
            }
        }
        self.styles.order += 1;
        Ok(())
    }
}

impl LayoutRules<'_> {
    fn parse(&mut self, parser: &mut Parser<'_, '_>) {
        let mut incomplete = false;
        for rule in StyleSheetParser::new(parser, self) {
            if let Err((_, source)) = rule {
                // These at-rules do not contain element declarations.
                let source = source.trim_start().to_ascii_lowercase();
                if !["@font-face", "@keyframes", "@-webkit-keyframes", "@page"]
                    .iter()
                    .any(|prefix| source.starts_with(prefix))
                {
                    incomplete = true;
                }
            }
        }
        self.styles.incomplete |= incomplete;
    }

    fn push_media(&mut self, media: Media) {
        for query in &media {
            if query.min <= query.max && query.max >= 1 {
                self.styles.breakpoints.extend(
                    [query.min, query.max.saturating_add(1)]
                        .into_iter()
                        .filter(|width| *width > 1 && *width < i32::MAX),
                );
            }
        }
        self.media.push(media);
    }
}

impl LayoutStyles {
    pub(crate) fn from_document(dom: &scraper::Html) -> Self {
        let mut styles = Self::default();
        for node in dom
            .root_element()
            .descendants()
            .filter_map(scraper::ElementRef::wrap)
        {
            if node.value().name() != "style" {
                continue;
            }
            let css = node.text().collect::<String>();
            let mut input = ParserInput::new(&css);
            let mut rules = LayoutRules {
                styles: &mut styles,
                media: Vec::new(),
                uncertain: false,
                depth: 0,
            };
            if let Some(media) = style_media(node.value()) {
                rules.push_media(media);
            }
            rules.parse(&mut Parser::new(&mut input));
        }
        styles.breakpoints.sort_unstable();
        styles.breakpoints.dedup();
        styles
    }

    pub(crate) fn declarations(&self, node: scraper::ElementRef<'_>) -> Vec<Declaration> {
        self.cascade(node, None).0
    }

    fn cascade(
        &self,
        node: scraper::ElementRef<'_>,
        width: Option<i32>,
    ) -> (Vec<Declaration>, bool) {
        let element = node.value();
        let keys = std::iter::once(SelectorAtom::Tag(element.name().into()))
            .chain(element.id().map(|id| SelectorAtom::Id(id.into())))
            .chain(
                element
                    .classes()
                    .map(|class| SelectorAtom::Class(class.into())),
            );
        let candidates = keys
            .filter_map(|key| self.indexed.get(&key))
            .flatten()
            .chain(&self.unindexed);
        let mut author = Vec::new();
        let mut incomplete = self.incomplete;
        for rule in candidates.filter(|rule| rule.selector.matches(node)) {
            let applies = rule
                .media
                .iter()
                .map(|queries| {
                    queries
                        .iter()
                        .map(|query| query.applies(width))
                        .max()
                        .unwrap_or(Applicability::No)
                })
                .min()
                .unwrap_or(Applicability::Yes);
            if applies == Applicability::No {
                continue;
            }
            incomplete |= rule.has_layout
                && (rule.uncertain
                    || rule.conditional
                    || applies == Applicability::Uncertain
                    || matches!(rule.selector, Selector::Complex(Err(_))));
            if applies == Applicability::Yes
                && matches!(
                    rule.selector,
                    Selector::Compound(_) | Selector::Complex(Ok(_))
                )
                && !rule.conditional
            {
                author.push(rule);
            }
        }
        author.sort_unstable_by_key(|rule| (rule.selector.specificity(), rule.order));
        let mut declarations: Vec<_> = author
            .into_iter()
            .flat_map(|rule| rule.declarations.iter().cloned())
            .collect();
        let inline = element.attr("style").map_or_else(Vec::new, |style| {
            let mut input = ParserInput::new(style);
            let mut parser = Parser::new(&mut input);
            match html::parse_stylesheet_declarations(&mut parser) {
                Ok(inline) => inline,
                Err(_) => {
                    incomplete = true;
                    html::style_declarations(element)
                }
            }
        });
        incomplete |= inline.iter().any(|rule| !can_model(rule));
        declarations.extend(inline);
        (declarations, incomplete)
    }
}

fn can_model(declaration: &Declaration) -> bool {
    let value = declaration.value.as_str();
    if neutral(declaration) {
        return true;
    }
    match declaration.name.as_str() {
        "display" => matches!(
            value,
            "none"
                | "block"
                | "inline"
                | "inline-block"
                | "flex"
                | "inline-flex"
                | "table"
                | "table-cell"
                | "table-row"
                | "contents"
                | "block flex"
                | "inline flex"
        ),
        "visibility" => matches!(value, "visible" | "hidden"),
        "overflow" | "overflow-y" | "overflow-x" => value
            .split_ascii_whitespace()
            .all(|value| matches!(value, "visible" | "hidden" | "clip" | "auto" | "scroll")),
        "font-size" => Length::parse(value).is_some(),
        "font" => shorthand_font(value).is_some(),
        "line-height" => LineHeight::parse(value, 16.).is_some(),
        "width" | "min-width" | "max-width" => {
            matches!(value, "auto" | "none") || Length::parse(value).is_some()
        }
        "height" | "min-height" | "max-height" => {
            matches!(value, "auto" | "none")
                || matches!(Length::parse(value), Some(Length::Px(_) | Length::Em(_)))
        }
        "flex-direction" => matches!(value, "row" | "row-reverse" | "column" | "column-reverse"),
        "flex-wrap" => matches!(value, "nowrap" | "wrap" | "wrap-reverse"),
        "box-sizing" => matches!(value, "border-box" | "content-box"),
        "border-collapse" => matches!(value, "collapse" | "separate"),
        "gap" | "row-gap" | "column-gap" | "border-spacing" => {
            value == "normal"
                || value
                    .split_ascii_whitespace()
                    .all(|length| Length::parse(length).is_some())
        }
        name if property_axis(name, "border").is_some_and(|axis| {
            [
                "",
                "top",
                "right",
                "bottom",
                "left",
                "block",
                "block-start",
                "block-end",
                "inline",
                "inline-start",
                "inline-end",
            ]
            .contains(
                &axis
                    .strip_suffix("-width")
                    .or_else(|| axis.strip_suffix("-style"))
                    .or_else(|| axis.strip_suffix("-color"))
                    .unwrap_or(axis),
            ) || matches!(axis, "width" | "style" | "color")
        }) =>
        {
            !value.contains("var(") && !value.contains("calc(")
        }
        "padding"
        | "margin"
        | "padding-top"
        | "padding-right"
        | "padding-bottom"
        | "padding-left"
        | "padding-block"
        | "padding-block-start"
        | "padding-block-end"
        | "padding-inline"
        | "padding-inline-start"
        | "padding-inline-end"
        | "margin-top"
        | "margin-right"
        | "margin-bottom"
        | "margin-left"
        | "margin-block"
        | "margin-block-start"
        | "margin-block-end"
        | "margin-inline"
        | "margin-inline-start"
        | "margin-inline-end" => {
            !value.is_empty()
                && value.split_ascii_whitespace().all(|value| {
                    Length::parse(value).is_some()
                        || declaration.name.starts_with("margin") && value == "auto"
                })
        }
        _ => false,
    }
}

fn neutral(declaration: &Declaration) -> bool {
    let name = declaration.name.as_str();
    name.starts_with("background")
        || name.starts_with("outline")
        || name.starts_with("text-decoration")
        || name.starts_with("mso-")
        || matches!(
            name,
            "font-family"
                | "font-weight"
                | "font-style"
                | "color"
                | "opacity"
                | "text-align"
                | "letter-spacing"
                | "text-transform"
                | "vertical-align"
                | "border-radius"
                | "box-shadow"
                | "justify-content"
                | "align-items"
                | "cursor"
        )
}

#[derive(Clone, Copy, Debug)]
enum Length {
    Px(f32),
    Em(f32),
    Percent(f32),
}

impl Length {
    fn parse(value: &str) -> Option<Self> {
        let value = value.trim();
        if let Some(pixels) = css_pixels(value) {
            return Some(Self::Px(pixels));
        }
        let (number, unit) = value
            .strip_suffix("em")
            .map(|number| (number, Self::Em as fn(f32) -> Self))
            .or_else(|| {
                value
                    .strip_suffix('%')
                    .map(|number| (number, Self::Percent as fn(f32) -> Self))
            })?;
        let number = number.parse::<f32>().ok()?;
        (number.is_finite() && number >= 0.).then(|| unit(number))
    }

    fn resolve(self, width: f32, font: f32) -> f32 {
        match self {
            Self::Px(pixels) => pixels,
            Self::Em(em) => em * font,
            Self::Percent(percent) => percent * 0.01 * width,
        }
    }
}

const ZERO: Length = Length::Px(0.);

#[derive(Clone, Copy, Debug)]
enum LineHeight {
    Normal,
    Ratio(f32),
    Px(f32),
}

impl LineHeight {
    fn parse(value: &str, size: f32) -> Option<Self> {
        if value == "normal" {
            return Some(Self::Normal);
        }
        if let Some(length) = Length::parse(value) {
            return Some(Self::Px(length.resolve(size, size)));
        }
        let ratio = value.parse::<f32>().ok()?;
        (ratio.is_finite() && ratio >= 0.).then_some(Self::Ratio(ratio))
    }

    fn resolve(self, size: f32, metrics: TextMetrics) -> f32 {
        match self {
            Self::Normal => (metrics.normal_line)(size),
            Self::Ratio(ratio) => ratio * size,
            Self::Px(pixels) => pixels,
        }
    }
}

#[derive(Clone, Copy)]
struct Font {
    size: f32,
    line: LineHeight,
    metrics: TextMetrics,
}

impl Font {
    fn for_element(self, name: &str, style: &[Declaration], zero: bool, uncertain: bool) -> Self {
        let scale = match name {
            "h1" => 2.,
            "h2" => 1.5,
            "h3" => 1.17,
            "h5" | "small" => 0.83,
            "h6" => 0.67,
            _ => 1.,
        };
        let default = self.size * scale;
        let mut size: Option<(bool, Option<Length>)> = None;
        let mut line: Option<(bool, &str)> = None;
        for declaration in style {
            let important = declaration.important;
            match declaration.name.as_str() {
                "font-size" if size.is_none_or(|(important, _)| !important) || important => {
                    size = Some((important, Length::parse(&declaration.value)));
                }
                "line-height" if line.is_none_or(|(important, _)| !important) || important => {
                    line = Some((important, declaration.value.as_str()));
                }
                "font" => {
                    if let Some((font_size, height)) = shorthand_font(&declaration.value) {
                        if important || size.is_none_or(|(important, _)| !important) {
                            size = Some((important, Some(font_size)));
                        }
                        if important || line.is_none_or(|(important, _)| !important) {
                            line = Some((important, height));
                        }
                    }
                }
                _ => {}
            }
        }
        let size = if zero {
            0.
        } else {
            let size = size
                .and_then(|(_, size)| size)
                .map_or(default, |size| size.resolve(self.size, self.size));
            if uncertain {
                size.max(default).max(16.)
            } else {
                size
            }
        };
        let line = line
            .and_then(|(_, line)| LineHeight::parse(line, size))
            .unwrap_or(self.line);
        let line = if uncertain {
            LineHeight::Px(
                line.resolve(size, self.metrics)
                    .max((self.metrics.normal_line)(size)),
            )
        } else {
            line
        };
        Self { size, line, ..self }
    }
}

#[derive(Debug)]
enum Layout {
    Text {
        em: f32,
        font: f32,
        line: f32,
    },
    Break {
        height: f32,
    },
    Image {
        width: f32,
        width_percent: Option<f32>,
        height: f32,
        min_height: f32,
        inline: bool,
        padding: [Length; 4],
        margin: [Length; 4],
        border: [f32; 4],
        font: f32,
    },
    Flow(Box<Flow>),
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum FlowKind {
    Block,
    InlineBlock,
    Cell,
    BlockCell,
    Stacked,
    Row,
    Table,
    TableRow,
}

#[derive(Debug)]
struct Flow {
    children: Vec<Layout>,
    kind: FlowKind,
    width: Option<Length>,
    min_width: Option<Length>,
    max_width: Option<Length>,
    font: f32,
    padding: [Length; 4],
    margin: [Length; 4],
    border: [f32; 4],
    gap: [Length; 2],
    /// Height hints include the element's own padding and border in border-box.
    border_box: bool,
    height: f32,
    min_height: f32,
    max_height: f32,
}

#[derive(Default)]
struct Measured {
    height: f32,
    above: f32,
    below: f32,
}

impl Measured {
    fn outer_height(&self) -> f32 {
        self.above + self.height + self.below
    }
}

#[derive(Clone, Copy)]
pub(super) struct TextMetrics {
    advance: fn(char) -> f32,
    normal_line: fn(f32) -> f32,
}

impl TextMetrics {
    pub(super) const DEFAULT: Self = Self {
        advance: approximate_advance,
        normal_line: |size| (size * 1.125).ceil(),
    };

    #[cfg(feature = "test-utils")]
    pub(super) const CALIBRATION: Self = Self {
        advance: calibration_advance,
        normal_line: calibration_line_height,
    };

    fn width(self, text: &str) -> f32 {
        let space = (self.advance)(' ');
        text.split_whitespace()
            .map(|word| word.chars().map(self.advance).sum::<f32>() + space)
            .sum::<f32>()
            - space
    }
}

fn approximate_advance(ch: char) -> f32 {
    match ch {
        '.' | ',' | '!' | ':' | ';' | '\'' | ' ' => 0.25,
        'i' | 'j' | 'l' | 't' => 0.28,
        'f' | 'r' => 0.33,
        's' => 0.4,
        'a' | 'c' | 'e' | 'z' => 0.45,
        'w' => 0.73,
        'm' => 0.78,
        'M' | 'W' => 0.89,
        'A'..='Z' => 0.64,
        '\0'..='\x7f' => 0.5,
        _ => 1.0,
    }
}

#[cfg(feature = "test-utils")]
// Renamed Latin and punctuation subset of Liberation Sans Regular, under OFL 1.1.
pub(super) const CALIBRATION_FONT: &[u8] =
    include_bytes!("../../fixtures/calibration-font/fell-calibration-sans.ttf");

#[cfg(feature = "test-utils")]
fn calibration_face() -> &'static ttf_parser::Face<'static> {
    static FONT: std::sync::OnceLock<ttf_parser::Face<'static>> = std::sync::OnceLock::new();
    FONT.get_or_init(|| ttf_parser::Face::parse(CALIBRATION_FONT, 0).unwrap())
}

#[cfg(feature = "test-utils")]
fn calibration_advance(ch: char) -> f32 {
    let font = calibration_face();
    font.glyph_index(ch)
        .and_then(|glyph| font.glyph_hor_advance(glyph))
        .map_or_else(
            || approximate_advance(ch),
            |advance| f32::from(advance) / f32::from(font.units_per_em()),
        )
}

#[cfg(feature = "test-utils")]
fn calibration_line_height(size: f32) -> f32 {
    let font = calibration_face();
    let scale = size / f32::from(font.units_per_em());
    // WebKit rounds the three font metrics separately for a normal line.
    (f32::from(font.ascender()) * scale).round()
        + (f32::from(-font.descender()) * scale).round()
        + (f32::from(font.line_gap()) * scale).round()
}

fn css_pixels(value: &str) -> Option<f32> {
    if html::is_zero_length(value) {
        return Some(0.);
    }
    let pixels = value.strip_suffix("px")?.parse::<f32>().ok()?;
    (pixels.is_finite() && pixels >= 0.).then_some(pixels)
}

fn html_pixels(value: &str) -> Option<f32> {
    let value = value.trim();
    let pixels = value
        .strip_suffix("px")
        .unwrap_or(value)
        .parse::<f32>()
        .ok()?;
    (pixels.is_finite() && pixels >= 0.).then_some(pixels)
}

fn edge_values<T: Copy>(
    axis: &str,
    mut lengths: impl Iterator<Item = Option<T>>,
) -> [Option<T>; 4] {
    let first = lengths.next().flatten();
    let second = lengths.next().unwrap_or(first);
    let third = lengths.next().unwrap_or(first);
    let fourth = lengths.next().unwrap_or(second);
    match axis {
        "" => [first, second, third, fourth],
        "top" | "block-start" => [first, None, None, None],
        "right" | "inline-end" => [None, first, None, None],
        "bottom" | "block-end" => [None, None, first, None],
        "left" | "inline-start" => [None, None, None, first],
        "block" => [first, None, second, None],
        "inline" => [None, second, None, first],
        _ => [None; 4],
    }
}

fn update_edges<T: Copy>(
    edges: &mut [Option<(bool, T)>; 4],
    values: [Option<T>; 4],
    important: bool,
) {
    for (edge, value) in edges.iter_mut().zip(values) {
        if let Some(value) = value
            && (important || edge.is_none_or(|(important, _)| !important))
        {
            *edge = Some((important, value));
        }
    }
}

fn property_axis<'a>(name: &'a str, property: &str) -> Option<&'a str> {
    let axis = name.strip_prefix(property)?;
    if axis.is_empty() {
        Some(axis)
    } else {
        axis.strip_prefix('-')
    }
}

fn box_edges(style: &[Declaration], property: &str) -> [Option<Length>; 4] {
    let mut edges = [None; 4];
    for declaration in style {
        if let Some(axis) = property_axis(&declaration.name, property) {
            update_edges(
                &mut edges,
                edge_values(
                    axis,
                    declaration
                        .value
                        .split_ascii_whitespace()
                        .map(Length::parse),
                ),
                declaration.important,
            );
        }
    }
    edges.map(|edge| edge.map(|(_, length)| length))
}

fn shorthand_font(value: &str) -> Option<(Length, &str)> {
    let mut words = value.split_ascii_whitespace().peekable();
    while let Some(word) = words.next() {
        let (size, line) = word.split_once('/').unwrap_or((word, "normal"));
        if let Some(size) = Length::parse(size)
            && words.peek().is_some()
        {
            return Some((size, line));
        }
    }
    None
}

fn border_length(value: &str) -> Option<f32> {
    match value {
        "thin" => Some(1.),
        "medium" => Some(3.),
        "thick" => Some(5.),
        _ => css_pixels(value),
    }
}

fn border_style(value: &str) -> Option<bool> {
    match value {
        "none" | "hidden" => Some(false),
        "solid" | "dashed" | "dotted" | "double" | "groove" | "ridge" | "inset" | "outset" => {
            Some(true)
        }
        _ => None,
    }
}

fn border_edges(style: &[Declaration]) -> [f32; 4] {
    let mut widths = [None; 4];
    let mut drawn = [None; 4];
    for declaration in style {
        let Some(axis) = property_axis(&declaration.name, "border") else {
            continue;
        };
        let words = declaration.value.split_ascii_whitespace();
        let important = declaration.important;
        if let Some(axis) = axis
            .strip_suffix("-width")
            .or((axis == "width").then_some(""))
        {
            update_edges(
                &mut widths,
                edge_values(axis, words.map(border_length)),
                important,
            );
        } else if let Some(axis) = axis
            .strip_suffix("-style")
            .or((axis == "style").then_some(""))
        {
            update_edges(
                &mut drawn,
                edge_values(axis, words.map(border_style)),
                important,
            );
        } else if !axis.ends_with("color") {
            let width = words.clone().find_map(border_length).unwrap_or(3.);
            let visible = words.filter_map(border_style).next().unwrap_or(false);
            update_edges(
                &mut widths,
                edge_values(axis, [Some(width)].into_iter()),
                important,
            );
            update_edges(
                &mut drawn,
                edge_values(axis, [Some(visible)].into_iter()),
                important,
            );
        }
    }
    std::array::from_fn(|edge| {
        if drawn[edge].is_some_and(|(_, drawn)| drawn) {
            widths[edge].map_or(3., |(_, width)| width)
        } else {
            0.
        }
    })
}

impl Layout {
    fn from_node(
        node: ego_tree::NodeRef<'_, Node>,
        styles: &LayoutStyles,
        width: Option<i32>,
        inherited: Font,
        inherited_uncertainty: bool,
    ) -> Option<Self> {
        let element = match node.value() {
            Node::Text(text) if inherited.size > 0. => {
                let em = inherited.metrics.width(text);
                return (em > 0.).then_some(Self::Text {
                    em,
                    font: inherited.size,
                    line: inherited.line.resolve(inherited.size, inherited.metrics),
                });
            }
            Node::Element(element) => element,
            _ => return None,
        };
        let name = element.name();
        if matches!(name, "head" | "script" | "style" | "title") {
            return None;
        }
        let (style, incomplete) = styles.cascade(scraper::ElementRef::wrap(node)?, width);
        let uncertain = inherited_uncertainty || incomplete;
        let display = html::declared_value(&style, "display");
        if !uncertain
            && (display == Some("none")
                || (display.is_none()
                    && element
                        .attr("hidden")
                        .is_some_and(|value| !value.eq_ignore_ascii_case("until-found"))))
        {
            return None;
        }
        let zero = !uncertain
            && html::declared_value(&style, "font").is_none()
            && html::has_zero_font_size(element, &style, inherited.size == 0.);
        let font = inherited.for_element(name, &style, zero, uncertain);
        if name == "br" {
            return (font.size > 0.).then_some(Self::Break {
                height: font.line.resolve(font.size, font.metrics),
            });
        }
        let length = |property| {
            html::declared_value(&style, property)
                .and_then(Length::parse)
                .and_then(|length| match length {
                    Length::Percent(_) => None,
                    _ => Some(length.resolve(0., font.size)),
                })
        };
        let padding = box_edges(&style, "padding");
        let margin = box_edges(&style, "margin");
        let border = border_edges(&style);
        let cell = matches!(name, "td" | "th");
        let padding = padding.map(|edge| edge.unwrap_or(Length::Px(if cell { 1. } else { 0. })));
        let default_margin = match name {
            "h1" => font.size * 0.67,
            "h2" => font.size * 0.83,
            "h3" => font.size,
            "h4" => font.size * 1.33,
            "h5" => font.size * 1.67,
            "h6" => font.size * 2.33,
            "p" | "pre" | "ul" | "ol" | "blockquote" => font.size,
            _ => 0.,
        };
        let margin = std::array::from_fn(|edge| {
            margin[edge].unwrap_or(if edge % 2 == 0 {
                Length::Px(default_margin)
            } else {
                ZERO
            })
        });
        let dimension = |name| {
            length(name)
                .or_else(|| element.attr(name).and_then(html_pixels))
                .filter(|value| *value > 0.)
        };
        let min_height = length("min-height").unwrap_or(0.);
        if name == "img" {
            let width = dimension("width");
            let height = dimension("height");
            let width_percent = match html::declared_value(&style, "width").and_then(Length::parse)
            {
                Some(Length::Percent(percent)) => Some(percent * 0.01),
                _ => None,
            };
            return Some(Self::Image {
                width: width
                    .or_else(|| height.map(|height| height * 1.5))
                    .unwrap_or(120.),
                width_percent,
                height: height
                    .or_else(|| width.map(|width| width / 1.5))
                    .unwrap_or(80.),
                min_height: if height.is_none()
                    && element.attr("src").is_some_and(|src| !src.is_empty())
                {
                    // A decoded local image can have any intrinsic ratio.
                    f32::INFINITY
                } else {
                    min_height
                },
                inline: !matches!(display, Some("block")),
                padding,
                margin,
                border,
                font: font.size,
            });
        }

        let direction = html::declared_value(&style, "flex-direction");
        let flex = matches!(
            display,
            Some("flex" | "inline-flex" | "block flex" | "inline flex")
        );
        let kind = match name {
            _ if flex && matches!(direction, Some("column" | "column-reverse")) => {
                FlowKind::Stacked
            }
            _ if flex
                && html::declared_value(&style, "flex-wrap")
                    .is_none_or(|wrap| wrap == "nowrap") =>
            {
                FlowKind::Row
            }
            _ if flex => FlowKind::Stacked,
            "table" => FlowKind::Table,
            "tr" => FlowKind::TableRow,
            "td" | "th" if display == Some("block") => FlowKind::BlockCell,
            "td" | "th" => FlowKind::Cell,
            "tbody" | "thead" | "tfoot" => FlowKind::Stacked,
            _ if display == Some("inline-block") => FlowKind::InlineBlock,
            _ if html::clips_vertical_overflow(&style) => FlowKind::Stacked,
            _ => FlowKind::Block,
        };
        let inline_flow = matches!(
            kind,
            FlowKind::Block | FlowKind::InlineBlock | FlowKind::Cell | FlowKind::BlockCell
        );
        let mut children: Vec<Self> = Vec::new();
        for child in node
            .children()
            .filter(|_| name != "svg")
            .filter_map(|child| Self::from_node(child, styles, width, font, uncertain))
        {
            match (children.last_mut(), child) {
                (
                    Some(Self::Text {
                        em,
                        font: text_font,
                        line,
                    }),
                    Self::Text {
                        em: next,
                        font: next_font,
                        line: next_line,
                    },
                ) if inline_flow && *text_font == next_font && *line == next_line => {
                    *em += (font.metrics.advance)(' ') + next;
                }
                (Some(Self::Text { .. }), Self::Break { .. }) if inline_flow => {
                    children.push(Self::Break { height: 0. });
                }
                (_, child) => children.push(child),
            }
        }
        if children.len() == 1
            && matches!(
                name,
                "a" | "b"
                    | "del"
                    | "em"
                    | "font"
                    | "i"
                    | "s"
                    | "small"
                    | "span"
                    | "strike"
                    | "strong"
                    | "u"
            )
            && !uncertain
            && style.iter().all(neutral)
        {
            return children.pop();
        }
        let attribute_height = if matches!(name, "td" | "th" | "tr" | "div") {
            element.attr("height").and_then(html_pixels).unwrap_or(0.)
        } else {
            0.
        };
        let height = length("height")
            .unwrap_or(attribute_height)
            .max(match name {
                "hr" => 20.,
                "svg" => dimension("height").unwrap_or(150.),
                _ => 0.,
            });
        let max_height = if !uncertain
            && matches!(name, "div" | "aside" | "section" | "article" | "p")
            && matches!(display, None | Some("block"))
            && html::clips_vertical_overflow(&style)
        {
            length("max-height").unwrap_or(f32::INFINITY)
        } else {
            f32::INFINITY
        };
        let gap = if kind == FlowKind::Table {
            let default = element
                .attr("cellspacing")
                .and_then(html_pixels)
                .unwrap_or(2.);
            if html::declared_value(&style, "border-collapse") == Some("collapse") {
                [ZERO; 2]
            } else {
                let [inline, block] =
                    authored_gap(&style, "border-spacing", "", "", Length::Px(default));
                [block, inline]
            }
        } else if flex {
            authored_gap(&style, "gap", "row-gap", "column-gap", ZERO)
        } else {
            [ZERO; 2]
        };
        Some(Self::Flow(Box::new(Flow {
            children,
            kind,
            width: html::declared_value(&style, "width")
                .and_then(Length::parse)
                .or_else(|| {
                    element.attr("width").and_then(|width| {
                        html_pixels(width)
                            .map(Length::Px)
                            .or_else(|| Length::parse(width))
                    })
                }),
            min_width: html::declared_value(&style, "min-width").and_then(Length::parse),
            max_width: html::declared_value(&style, "max-width").and_then(Length::parse),
            font: font.size,
            padding,
            margin,
            border,
            gap,
            border_box: html::declared_value(&style, "box-sizing") == Some("border-box"),
            height,
            min_height,
            max_height,
        })))
    }

    fn measure(&self, width: f32, image_limit: f32) -> Measured {
        match self {
            Self::Text { em, font, line } => Measured {
                height: (em * font / width.max(1.)).ceil() * line,
                ..Measured::default()
            },
            Self::Break { height } => Measured {
                height: *height,
                ..Measured::default()
            },
            Self::Image {
                width: natural,
                width_percent,
                height,
                min_height,
                padding,
                margin,
                border,
                font,
                ..
            } => {
                let [top, right, bottom, left] = insets(*padding, *border, width, *font);
                let available = (width - left - right).max(1.);
                let drawn = width_percent.map_or(*natural, |percent| width * percent);
                Measured {
                    height: (height * (drawn.min(available) / natural))
                        .max(*min_height)
                        .min(image_limit)
                        + top
                        + bottom,
                    above: margin[0].resolve(width, *font),
                    below: margin[2].resolve(width, *font),
                }
            }
            Self::Flow(flow) => flow.measure(width, image_limit, false),
        }
    }

    fn inline_width(&self) -> f32 {
        match self {
            Self::Text { em, font, .. } => em * font,
            Self::Image { width, .. } => *width,
            Self::Flow(flow) => flow
                .children
                .iter()
                .map(Self::inline_width)
                .fold(1., f32::max),
            Self::Break { .. } => 1.,
        }
    }

    fn inline_box_width(&self, width: f32) -> Option<f32> {
        if let Self::Image {
            width: natural,
            width_percent,
            inline: true,
            padding,
            margin,
            border,
            font,
            ..
        } = self
        {
            let [_, right, _, left] = insets(*padding, *border, width, *font);
            let drawn = width_percent.map_or(*natural, |percent| width * percent);
            let margins = margin[1].resolve(width, *font) + margin[3].resolve(width, *font);
            return Some((drawn + left + right).min(width) + margins);
        }
        let Self::Flow(flow) = self else {
            return None;
        };
        if flow.kind != FlowKind::InlineBlock {
            return None;
        }
        let resolve = |length: Length| length.resolve(width, flow.font);
        let declared = resolve(flow.width?)
            .min(flow.max_width.map_or(f32::INFINITY, resolve))
            .max(flow.min_width.map_or(0., resolve));
        let [_, right, _, left] = insets(flow.padding, flow.border, width, flow.font);
        let margins = resolve(flow.margin[1]) + resolve(flow.margin[3]);
        Some(declared + margins + if flow.border_box { 0. } else { left + right })
    }
}

fn insets(padding: [Length; 4], border: [f32; 4], width: f32, font: f32) -> [f32; 4] {
    std::array::from_fn(|edge| border[edge] + padding[edge].resolve(width, font))
}

fn authored_gap(
    style: &[Declaration],
    shorthand: &str,
    row: &str,
    column: &str,
    default: Length,
) -> [Length; 2] {
    let mut gaps: [Option<(bool, Length)>; 2] = [None; 2];
    for declaration in style {
        let mut words = declaration.value.split_ascii_whitespace();
        let first = words.next().and_then(Length::parse);
        let values = match declaration.name.as_str() {
            name if name == shorthand => [first, words.next().and_then(Length::parse).or(first)],
            name if name == row => [first, None],
            name if name == column => [None, first],
            _ => continue,
        };
        for (gap, value) in gaps.iter_mut().zip(values) {
            if let Some(value) = value
                && (declaration.important || gap.is_none_or(|(important, _)| !important))
            {
                *gap = Some((declaration.important, value));
            }
        }
    }
    gaps.map(|gap| gap.map_or(default, |(_, length)| length))
}

impl Flow {
    fn inline_lines(&self, width: f32, image_limit: f32) -> impl Iterator<Item = Measured> + '_ {
        let mut children = self.children.iter().peekable();
        std::iter::from_fn(move || {
            let child = children.next()?;
            let line = child.measure(width, image_limit);
            let Some(mut used) = child.inline_box_width(width) else {
                return Some(line);
            };
            let mut tallest = line.outer_height();
            while let Some(next) = children.peek()
                && let Some(next_width) = next.inline_box_width(width)
                && used + next_width <= width
            {
                used += next_width;
                tallest = tallest.max(next.measure(width, image_limit).outer_height());
                children.next();
            }
            Some(Measured {
                height: tallest,
                ..Measured::default()
            })
        })
    }

    fn measure(&self, width: f32, image_limit: f32, in_table_row: bool) -> Measured {
        let [top, right, bottom, left] = insets(self.padding, self.border, width, self.font);
        let [above, margin_right, below, margin_left] =
            self.margin.map(|edge| edge.resolve(width, self.font));
        let natural_width = (width - left - right - margin_left - margin_right).max(1.);
        let content_width = |hint: Length| {
            let inset = if self.border_box { left + right } else { 0. };
            (hint.resolve(width, self.font) - inset).max(1.)
        };
        let available = self
            .width
            // The row already allocated a share of the table to the cell.
            // Resolving a percentage again would narrow every nested box.
            .filter(|width| !in_table_row || !matches!(width, Length::Percent(_)))
            .map_or(natural_width, content_width)
            .min(self.max_width.map_or(
                if self.kind == FlowKind::BlockCell {
                    f32::INFINITY
                } else {
                    natural_width
                },
                content_width,
            ))
            .max(self.min_width.map_or(1., content_width));
        let [row_gap, column_gap] = self.gap.map(|gap| gap.resolve(available, self.font));
        let mut measured = Measured {
            height: 0.,
            above,
            below,
        };
        let content = match self.kind {
            FlowKind::TableRow
                if self.children.iter().all(
                    |child| matches!(child, Layout::Flow(flow) if flow.kind == FlowKind::BlockCell),
                ) =>
            {
                // Responsive newsletters switch their cells to full-width
                // blocks. The browser stacks them inside an anonymous cell.
                self.children
                    .iter()
                    .map(|child| child.measure(available, image_limit).outer_height())
                    .sum()
            }
            FlowKind::Row | FlowKind::TableRow => {
                let count = self.children.len();
                let width = (available - column_gap * count.saturating_sub(1) as f32).max(1.);
                let preferred: f32 = self.children.iter().map(Layout::inline_width).sum();
                self.children
                    .iter()
                    .map(|child| {
                        let share = if self.kind == FlowKind::TableRow {
                            width / count as f32
                        } else if preferred > width {
                            width * child.inline_width() / preferred
                        } else {
                            width
                        };
                        match child {
                            Layout::Flow(flow) if self.kind == FlowKind::TableRow => {
                                flow.measure(share, image_limit, true)
                            }
                            _ => child.measure(share, image_limit),
                        }
                        .outer_height()
                    })
                    .fold(0., f32::max)
            }
            FlowKind::Block => {
                let mut previous = 0_f32;
                let mut height = 0.;
                for (index, line) in self.inline_lines(available, image_limit).enumerate() {
                    if index == 0 && top == 0. {
                        measured.above = measured.above.max(line.above);
                    } else {
                        height += previous.max(line.above);
                    }
                    height += line.height;
                    previous = line.below;
                }
                if bottom == 0. && self.height == 0. && self.min_height == 0. {
                    measured.below = measured.below.max(previous);
                    height
                } else {
                    height + previous
                }
            }
            FlowKind::InlineBlock | FlowKind::Cell | FlowKind::BlockCell => self
                .inline_lines(available, image_limit)
                .map(|line| line.outer_height())
                .sum(),
            FlowKind::Stacked | FlowKind::Table => {
                let table = self.kind == FlowKind::Table;
                let width = if table {
                    (available - 2. * column_gap).max(1.)
                } else {
                    available
                };
                let boxes = self
                    .children
                    .iter()
                    .map(|child| child.measure(width, image_limit).outer_height());
                let rows = if table {
                    self.children
                        .iter()
                        .map(|child| match child {
                            Layout::Flow(flow) if flow.kind == FlowKind::Stacked => {
                                flow.children.len()
                            }
                            _ => 1,
                        })
                        .sum::<usize>()
                        + 1
                } else {
                    self.children.len().saturating_sub(1)
                };
                boxes.sum::<f32>() + row_gap * rows as f32
            }
        };
        let insets = top + bottom;
        let border_box = if self.border_box { insets } else { 0. };
        let minimum = self.min_height.max(0.);
        measured.height = content
            .max(self.height.max(minimum) - border_box)
            .min(self.max_height.max(minimum) - border_box)
            .max(0.)
            + insets;
        measured
    }
}

#[cfg(test)]
#[test]
fn descendant_dimensions_win_by_specificity() {
    let mail = WebMail::prepare(concat!(
        "<style>.wrap .hero{height:280px}.hero{height:20px}</style>",
        r#"<div class="wrap"><div class="hero"></div></div>"#,
    ));
    assert_eq!(mail.estimated_height(440.), 280. + HEIGHT_ALLOWANCE);
}

#[cfg(test)]
#[test]
fn structural_selectors_and_negated_hover_apply_at_rest() {
    let mail = WebMail::prepare(concat!(
        "<style>.wrap > .hero{height:280px} .wrap h1{margin:0;font-size:32px;line-height:40px} ",
        "td.section:first-child{padding-top:80px} .idle:not(:hover){height:300px} ",
        ".idle:hover{height:999px} @media(max-width:400px){.wrap > .hero{height:120px}}</style>",
        r#"<div class="wrap"><div class="hero"></div><h1>News</h1></div>"#,
        r#"<table cellspacing="0"><tr><td class="section">In town</td></tr></table>"#,
        r#"<div class="idle"></div>"#,
    ));
    assert_eq!(
        mail.estimated_height(440.),
        280. + 40. + 99. + 300. + HEIGHT_ALLOWANCE
    );
    assert_eq!(
        mail.estimated_height(360.),
        120. + 40. + 99. + 300. + HEIGHT_ALLOWANCE
    );
}

#[cfg(test)]
#[test]
fn inline_blocks_share_height_until_they_wrap() {
    let mail = WebMail::prepare(concat!(
        "<style>.col{display:inline-block;width:48%;height:65px} ",
        ".tall{height:100px} .fixed{width:160px} .unknown{width:auto} ",
        "@media(max-width:400px){.col{display:block;width:100%}}</style>",
        r#"<div class="col"></div> <div class="col tall"></div>"#,
        r#"<div class="col fixed"></div><div class="col unknown"></div>"#,
    ));
    assert_eq!(
        mail.estimated_height(600.),
        100. + 65. + 65. + HEIGHT_ALLOWANCE
    );
    assert_eq!(
        mail.estimated_height(360.),
        100. + 3. * 65. + HEIGHT_ALLOWANCE
    );
}

#[cfg(test)]
#[test]
fn height_estimates_account_for_headings_image_hints_and_table_rows() {
    let mail = WebMail::prepare(
        r#"<table><tr><td><h1>Today</h1></td><td><h1>News</h1></td></tr><tr><td colspan="2"><img width="300" src="https://example.test/photo"></td></tr></table>"#,
    );
    let height = mail.estimated_height(420.);
    assert!((300. ..330.).contains(&height), "{height}");
    assert!(mail.estimated_height(180.) < height);
}

#[cfg(test)]
#[test]
fn height_estimates_cap_image_hints_and_total_layout() {
    let image = Layout::Image {
        width: 1.,
        width_percent: None,
        height: 10_000_000.,
        min_height: 0.,
        inline: true,
        padding: [ZERO; 4],
        margin: [ZERO; 4],
        border: [0.; 4],
        font: 16.,
    };
    assert_eq!(
        image
            .measure(440., WebMail::image_height_limit(440.))
            .outer_height(),
        660.
    );
    let mail =
        WebMail::prepare(r#"<img width="600" height="200000" src="https://example.test/large">"#);
    assert_eq!(mail.estimated_height(440.), 660. + HEIGHT_ALLOWANCE);
    assert_eq!(mail.estimated_height(300.), 450. + HEIGHT_ALLOWANCE);
    assert_eq!(
        WebMail::prepare(&"<p>Another line</p>".repeat(500)).estimated_height(440.),
        MAX_ESTIMATED_HEIGHT
    );
}

#[cfg(test)]
#[test]
fn height_estimates_ignore_hidden_text_but_keep_visible_descendants() {
    let hidden = "x".repeat(50_000);
    let visible =
        r#"<p>Story</p><span style="font-size:16px">More</span><img width="80" height="60">"#;
    let html = format!(
        r#"<p>Story</p><div style="display:none">{hidden}</div>
        <div style="max-height:0;overflow:hidden">{hidden}</div>
        <div style="font-size:0">{hidden}<span style="font-size:16px">More</span><img width="80" height="60"></div>"#
    );
    assert_eq!(
        WebMail::prepare(&html).estimated_height(440.),
        WebMail::prepare(visible).estimated_height(440.)
    );
}

#[cfg(test)]
#[test]
fn height_estimates_respect_stylesheets_escapes_and_clipped_height() {
    let hidden = "x".repeat(80_000);
    let html = format!(
        r#"<style>
            div {{ display: none !important }}
            #zero {{ font-size: 0 }}
            .clipped {{ max-height: 10px; overflow: hidden }}
        </style>
        <p>Hi</p><div style="display:block">{hidden}</div>
        <aside id="zero">{hidden}</aside>
        <aside style="display:\6e one">{hidden}</aside>
        <aside class="clipped">{hidden}</aside>
        <aside style="max-height:10px;overflow:hidden">{hidden}</aside>"#
    );
    assert_eq!(WebMail::prepare(&html).estimated_height(440.), 82.);
}

#[cfg(test)]
#[test]
fn height_estimates_keep_stylesheet_overrides_and_visible_overflow() {
    let visible = "x".repeat(500);
    let html = format!(
        r#"<style>
            #first, #second {{ visibility: visible }}
            .hide {{ visibility: hidden }}
            .show {{ display: block }}
            .hide {{ display: none }}
        </style>
        <p id="first" class="hide" style="display:block">{visible}</p>
        <p class="hide show" id="second" style="display:block">{visible}</p>
        <p style="max-height:0">{visible}</p>
        <p style="visibility:hidden !important">{visible}</p>"#
    );
    assert_eq!(
        WebMail::prepare(&html).estimated_height(440.),
        WebMail::prepare(&format!("<p>{visible}</p>").repeat(4)).estimated_height(440.)
    );
}

#[cfg(test)]
#[test]
fn uncertain_rules_keep_matching_hidden_content_without_inflating_other_paragraphs() {
    let visible = "<p>Story</p>".repeat(8);
    let hidden = r#"<div style="display:none;font-size:0"><span>Author rules may reveal this text.</span></div>"#;
    let baseline =
        WebMail::prepare(&format!("<style>p {{margin:0}}</style>{visible}")).estimated_height(440.);
    for unmodeled in [
        "header .maybe {all:revert}",
        ".maybe::before {content:'x'}",
        "@supports (display:grid) {.maybe {display:block !important}}",
        "@media (prefers-color-scheme: dark) {.maybe {display:block !important}}",
        ".maybe {padding-unknown:10px}",
    ] {
        let html = format!("<style>p {{margin:0}} {unmodeled}</style>{visible}{hidden}");
        assert_eq!(
            WebMail::prepare(&html).estimated_height(440.),
            baseline,
            "{unmodeled}"
        );
        let html = html.replace("<div style", "<div class='maybe' style");
        let html = html.replace("<div class='maybe'", "<header><div class='maybe'") + "</header>";
        let estimate = WebMail::prepare(&html).estimated_height(440.);
        assert!(
            (baseline + 18. ..=baseline + 25.).contains(&estimate),
            "{unmodeled}: {estimate}"
        );
    }
    let inline = hidden.replace("display:none;", "all:revert;display:none;");
    assert!(WebMail::prepare(&inline).estimated_height(440.) > HEIGHT_ALLOWANCE);
}

#[cfg(test)]
#[test]
fn web_style_media_is_shared_by_the_estimator_and_the_page() {
    let mail = WebMail::prepare(concat!(
        r#"<style media="print">.box {display:none}</style>"#,
        r#"<style media="(min-width: 1px)">.box {height:100px}</style>"#,
        r#"<style media=" SCREEN ">.box {padding-block:8px}</style>"#,
        r#"<style media="all">.box {margin-block:10px}</style>"#,
        r#"<div class="box">Story</div>"#,
    ));
    let page = mail.page("light", |_| None);
    assert!(!page.contains("display:none"));
    assert!(page.contains(r#"<style media="(min-width: 1px)">.box {height:100px}"#));
    assert!(page.contains("padding-block:8px"));
    assert!(page.contains("margin-block:10px"));
    assert_eq!(mail.estimated_height(440.), 148.);
}

#[cfg(test)]
#[test]
fn height_estimates_keep_author_reveals_and_spacer_boxes() {
    let reveal = WebMail::prepare(
        r#"<style>.show {display:block !important}</style><div class="show" hidden>Story</div>"#,
    );
    assert_eq!(reveal.estimated_height(440.), 30.);

    let mail = WebMail::prepare(concat!(
        "<style>.show {display:block !important}</style>",
        r#"<body style="padding-block:6px 8px"><div hidden class="show">Story</div>"#,
        r#"<table><tr height="36"><td height="24" style="font-size:0"></td></tr></table>"#,
        r#"<div height="40" style="height:30px;min-height:50px;border-top:3px solid;border-bottom:4px solid;padding-block:8px 9px;margin-block:5px 6px"></div>"#,
        "</body>",
    ));
    assert_eq!(mail.estimated_height(440.), 169.);
}
