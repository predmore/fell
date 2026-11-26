use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    ops::Range,
    process::{Command, Stdio},
    rc::Rc,
    sync::Arc,
};

use gpui::{
    AnyElement, App, AvailableSpace, Bounds, ClipboardItem, CursorStyle, ElementId, FocusHandle,
    MouseButton, Pixels, SharedString, Window, div, point, prelude::*, px, size,
};

use super::{MailText, Mark, SOFT_HYPHEN, paint};
use crate::view::reader_links::{self, LinkPreview, Placement};

pub(super) fn checked_target(href: &str) -> Option<Arc<str>> {
    type Targets = HashMap<String, Option<Arc<str>>>;
    thread_local! { static TARGETS: RefCell<Targets> = RefCell::default(); }
    if href.len() > 4096 {
        return fell_render::reader_link_target(href).map(Arc::from);
    }
    TARGETS.with_borrow_mut(|cache| {
        if let Some(target) = cache.get(href) {
            return target.clone();
        }
        if cache.len() >= 2048 {
            cache.clear();
        }
        let target = fell_render::reader_link_target(href).map(Arc::from);
        cache.insert(href.into(), target.clone());
        target
    })
}

pub(super) struct LinkSpan {
    pub range: Range<usize>,
    pub target: Arc<str>,
    source: usize,
}

#[derive(Clone)]
pub(super) struct LinkMark {
    pub source: usize,
    pub target: Arc<str>,
}

pub(super) fn link_spans(runs: &[(usize, Mark)]) -> Vec<LinkSpan> {
    let mut links: Vec<LinkSpan> = Vec::new();
    let mut at = 0;
    for (len, mark) in runs {
        let end = at + len;
        if let Some(link) = &mark.link {
            if let Some(last) = links.last_mut()
                && last.range.end == at
                && last.source == link.source
            {
                last.range.end = end;
            } else {
                links.push(LinkSpan {
                    range: at..end,
                    target: link.target.clone(),
                    source: link.source,
                });
            }
        }
        at = end;
    }
    links
}

#[derive(Default)]
pub(super) struct LinkFocus {
    text: SharedString,
    handles: Vec<Option<LinkState>>,
}

#[derive(Default)]
pub(super) struct LinkControls {
    pub elements: Vec<AnyElement>,
    pub focused: Option<Range<usize>>,
}

struct LinkState {
    target: Arc<str>,
    handle: FocusHandle,
    place: Rc<Placement>,
}

fn link_preview(label: &str, target: &str) -> Option<LinkPreview> {
    if label == target {
        return None;
    }
    let (destination, mismatch) = crate::compose_ui::reader_link_destination(label, target)?;
    Some(LinkPreview {
        destination: destination.into(),
        target: target.to_owned().into(),
        mismatch,
    })
}

fn outline_span(rect: &Bounds<Pixels>) -> Range<Pixels> {
    rect.left() - px(2.)..rect.right() + px(2.)
}

/// Horizontal edges stop where two successive fragments touch.
fn outline_segments(
    span: Range<Pixels>,
    adjacent: Option<Range<Pixels>>,
) -> impl Iterator<Item = Range<Pixels>> {
    let segments = if let Some(covered) = adjacent {
        [
            span.start..span.end.min(covered.start),
            span.start.max(covered.end)..span.end,
        ]
    } else {
        [span, px(0.)..px(0.)]
    };
    segments.into_iter().filter(|edge| edge.start < edge.end)
}

pub(super) fn link_controls(
    mail: &MailText,
    bounds: Bounds<Pixels>,
    focus: &mut LinkFocus,
    window: &mut Window,
    cx: &mut App,
) -> LinkControls {
    if focus.text != mail.text {
        focus.text = mail.text.clone();
        focus.handles.clear();
    }
    focus.handles.resize_with(mail.links.len(), || None);
    let mut fragments: Vec<Vec<Bounds<Pixels>>> =
        (0..mail.links.len()).map(|_| Vec::new()).collect();
    mail.for_visible_line(bounds, window, None, |line, shaped, origin, _| {
        let layout = &shaped.unwrapped_layout;
        let glyphs = layout.runs.iter().flat_map(|run| &run.glyphs);
        let mut glyphs = glyphs.peekable();
        let source = &mail.text[line.range.clone()];
        let mut soft_hyphens = source.match_indices(SOFT_HYPHEN).peekable();
        let mut skipped = 0;
        let first = mail
            .links
            .partition_point(|link| link.range.end <= line.range.start);
        let last = mail
            .links
            .partition_point(|link| link.range.start < line.range.end);
        if first == last {
            return;
        }
        while let Some(glyph) = glyphs.next() {
            while let Some((_, hyphen)) =
                soft_hyphens.next_if(|(at, _)| *at <= glyph.index + skipped)
            {
                skipped += hyphen.len();
            }
            let byte = line.range.start + glyph.index + skipped;
            let links = &mail.links[first..last];
            let relative = links.partition_point(|link| link.range.end <= byte);
            if links
                .get(relative)
                .is_none_or(|link| !link.range.contains(&byte))
            {
                continue;
            }
            let right = glyphs.peek().map_or(layout.width, |next| next.position.x);
            if right <= glyph.position.x {
                continue;
            }
            let rect = Bounds {
                origin: point(origin.x + glyph.position.x, origin.y),
                size: size(right - glyph.position.x, mail.line_height),
            };
            let rectangles = &mut fragments[first + relative];
            if let Some(previous) = rectangles.last_mut()
                && previous.origin.y == rect.origin.y
                && previous.right() == rect.left()
            {
                previous.size.width = rect.right() - previous.left();
            } else {
                rectangles.push(rect);
            }
        }
    });

    let mut controls = LinkControls::default();
    for (index, (link, rects)) in mail.links.iter().zip(fragments).enumerate() {
        let state = link_state(
            &mut focus.handles[index],
            &link.target,
            mail.text[link.range.clone()].trim(),
            window,
            cx,
        );
        let first_line = mail
            .wrapped
            .borrow()
            .lines
            .partition_point(|line| line.range.end <= link.range.start);
        let top = bounds.top() + mail.line_height * first_line as f32;
        state
            .place
            .line
            .set((top.into(), (top + mail.line_height).into()));
        if state.handle.is_focused(window) {
            controls.focused = Some(link.range.clone());
        }
        let mut control =
            positioned_link(index, state, &rects, bounds, mail.palette.accent, window);
        control.layout_as_root(
            size(AvailableSpace::MaxContent, AvailableSpace::MaxContent),
            window,
            cx,
        );
        control.prepaint_at(bounds.origin, window, cx);
        controls.elements.push(control);
    }
    controls
}

fn link_state<'a>(
    slot: &'a mut Option<LinkState>,
    target: &Arc<str>,
    label: &str,
    window: &Window,
    cx: &mut App,
) -> &'a LinkState {
    if slot.as_ref().is_some_and(|state| &state.target != target) {
        *slot = None;
    }
    slot.get_or_insert_with(|| {
        let handle = cx.focus_handle().tab_stop(true);
        let place = Rc::new(Placement {
            line: Cell::default(),
            visible: Cell::default(),
            target: target.clone(),
            preview: link_preview(label, target),
        });
        reader_links::register(window, cx, &handle, &place);
        LinkState {
            target: target.clone(),
            handle,
            place,
        }
    })
}

fn positioned_link(
    index: usize,
    state: &LinkState,
    rects: &[Bounds<Pixels>],
    bounds: Bounds<Pixels>,
    accent: u32,
    window: &Window,
) -> AnyElement {
    let mask = window.content_mask().bounds;
    let clip = mask.intersect(&bounds);
    let visible = |rect: &Bounds<Pixels>| !rect.intersect(&clip).is_empty();
    state.place.visible.set(rects.iter().any(visible));
    let handle = &state.handle;
    let focused = handle.is_focused(window);
    let target = state.target.clone();
    div()
        .relative()
        .size_0()
        .id(ElementId::named_usize("mail-link", index))
        .track_focus(handle)
        .on_key_down(move |event, window, cx| {
            if event.is_held || event.keystroke.modifiers != gpui::Modifiers::default() {
                return;
            }
            match event.keystroke.key.as_str() {
                "enter" | "return" | "o" => open(target.clone(), window, cx),
                "y" => {
                    cx.write_to_clipboard(ClipboardItem::new_string(target.to_string()));
                    reader_links::notice(window, cx, "Copied link", false);
                }
                _ => return,
            }
            cx.stop_propagation();
        })
        .children(
            rects
                .iter()
                .enumerate()
                .filter(|(_, rect)| visible(rect))
                .map(|(at, rect)| {
                    let target = state.target.clone();
                    let handle = handle.clone();
                    let hover = handle.clone();
                    div()
                        .absolute()
                        .left(rect.left() - bounds.left())
                        .top(rect.top() - bounds.top())
                        .w(rect.size.width)
                        .h(rect.size.height)
                        .id(ElementId::named_usize("link-fragment", at))
                        .cursor(CursorStyle::PointingHand)
                        .when(focused, |hit| {
                            hit.child(fragment_outline(
                                rects,
                                at,
                                mask.left()..mask.right(),
                                paint(accent),
                            ))
                        })
                        .on_hover(move |hovered, window, cx| {
                            reader_links::hover(window, cx, &hover, *hovered)
                        })
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(move |_, window, cx| {
                            reader_links::clicked(window, cx);
                            window.focus(&handle);
                            open(target.clone(), window, cx);
                            cx.stop_propagation();
                        })
                }),
        )
        .into_any_element()
}

/// Focusable links placed over the text WebKit already painted.
pub(crate) struct SnapshotLinks {
    pub id: ElementId,
    pub links: Arc<[crate::render::webview::WebLink]>,
    pub width: f32,
    pub height: f32,
    pub drawn_width: f32,
    pub accent: u32,
}

impl IntoElement for SnapshotLinks {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl gpui::Element for SnapshotLinks {
    type RequestLayoutState = ();
    type PrepaintState = Vec<AnyElement>;

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&gpui::GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (gpui::LayoutId, ()) {
        let style = gpui::Style {
            position: gpui::Position::Absolute,
            inset: gpui::Edges::all(px(0.).into()),
            size: size(px(self.drawn_width).into(), px(self.height).into()),
            ..Default::default()
        };
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        id: Option<&gpui::GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Vec<AnyElement> {
        let scale = f32::from(bounds.size.width) / self.width;
        window.with_element_state(id.expect("snapshot links have an ID"), |state, window| {
            let mut states: Vec<Option<LinkState>> = state.unwrap_or_default();
            states.resize_with(self.links.len(), || None);
            let controls = self
                .links
                .iter()
                .zip(&mut states)
                .enumerate()
                .filter_map(|(index, (link, slot))| {
                    let rects: Vec<_> = link
                        .rects
                        .iter()
                        .map(|&[x, y, w, h]| {
                            Bounds {
                                origin: bounds.origin + point(px(x * scale), px(y * scale)),
                                size: size(px(w * scale), px(h * scale)),
                            }
                            .intersect(&bounds)
                        })
                        .filter(|rect| !rect.is_empty())
                        .collect();
                    let first = rects.first()?;
                    let target = checked_target(&link.href)?;
                    let state = link_state(slot, &target, &link.label, window, cx);
                    state
                        .place
                        .line
                        .set((first.top().into(), first.bottom().into()));
                    let mut control =
                        positioned_link(index, state, &rects, bounds, self.accent, window);
                    control.layout_as_root(
                        size(AvailableSpace::MaxContent, AvailableSpace::MaxContent),
                        window,
                        cx,
                    );
                    control.prepaint_at(bounds.origin, window, cx);
                    Some(control)
                })
                .collect();
            (controls, states)
        })
    }

    fn paint(
        &mut self,
        _: Option<&gpui::GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        controls: &mut Vec<AnyElement>,
        window: &mut Window,
        cx: &mut App,
    ) {
        for control in controls {
            control.paint(window, cx);
        }
    }
}

fn fragment_outline(
    rects: &[Bounds<Pixels>],
    at: usize,
    mask: Range<Pixels>,
    accent: gpui::Rgba,
) -> gpui::Div {
    let rect = &rects[at];
    let visible_span = |rect| {
        let span = outline_span(rect);
        span.start.max(mask.start)..span.end.min(mask.end)
    };
    let span = visible_span(rect);
    let successive = |upper: &Bounds<Pixels>, lower: &Bounds<Pixels>| {
        f32::from(lower.top() - upper.bottom()).abs() < 0.01
    };
    let prev = at
        .checked_sub(1)
        .and_then(|at| rects.get(at))
        .filter(|prev| successive(prev, rect));
    let next = rects.get(at + 1).filter(|next| successive(rect, next));
    let edges = div()
        .absolute()
        .top_0()
        .bottom_0()
        .left(span.start - rect.left())
        .w(span.end - span.start);
    if prev.is_none() && next.is_none() {
        return edges.border_1().border_color(accent).rounded(px(2.));
    }
    let rule = |edge: Range<Pixels>| {
        // Meet the next vertical stroke at each inside corner.
        let start = if edge.start > span.start {
            edge.start - px(1.)
        } else {
            edge.start
        };
        let end = if edge.end < span.end {
            edge.end + px(1.)
        } else {
            edge.end
        };
        div()
            .absolute()
            .left(start - span.start)
            .w(end - start)
            .h(px(1.))
            .bg(accent)
    };
    let side = || div().absolute().top_0().bottom_0().w(px(1.)).bg(accent);
    edges
        .child(side().left_0())
        .child(side().right_0())
        .children(
            outline_segments(span.clone(), prev.map(visible_span)).map(|edge| rule(edge).top_0()),
        )
        .children(
            outline_segments(span.clone(), next.map(visible_span))
                .map(|edge| rule(edge).bottom_0()),
        )
}

fn open_command(target: &str) -> Command {
    let mut command = Command::new("xdg-open");
    command
        .arg(target)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

fn open(target: Arc<str>, window: &Window, cx: &mut App) {
    let task = cx.background_executor().spawn(async move {
        open_command(&target)
            .status()
            .is_ok_and(|status| status.success())
    });
    window
        .spawn(cx, async move |cx| {
            if !task.await {
                fell_core::diagnostic!(WARN, "Could not open link");
                let _ = cx.update(|window, cx| {
                    reader_links::notice(window, cx, reader_links::OPEN_FAILED, true)
                });
            }
        })
        .detach();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text_render::PlainText;
    use fell_render::{Block, Inline, html_to_document};

    #[test]
    fn a_snapshot_text_link_can_receive_focus_and_copy_its_destination() {
        struct Preview;
        impl gpui::Render for Preview {
            fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
                div()
                    .relative()
                    .w(px(240.))
                    .h(px(80.))
                    .child(SnapshotLinks {
                        id: "snapshot-links".into(),
                        links: Arc::new([crate::render::webview::WebLink {
                            href: "https://example.test/route".into(),
                            label: "Harbour office".into(),
                            rects: vec![[12., 16., 130., 22.]],
                        }]),
                        width: 240.,
                        height: 80.,
                        drawn_width: 240.,
                        accent: 0x2873ef,
                    })
            }
        }

        let mut app = gpui::TestAppContext::single();
        let preview = app.add_window(|_, _| Preview);
        let window_handle: gpui::AnyWindowHandle = preview.into();
        window_handle
            .update(&mut app, |_, window, cx| {
                window.draw(cx).clear();
                window.focus_next();
                assert!(window.focused(cx).is_some());
            })
            .unwrap();
        app.simulate_keystrokes(preview.into(), "y");
        assert_eq!(
            app.read_from_clipboard()
                .and_then(|item| item.text())
                .as_deref(),
            Some("https://example.test/route"),
        );
    }

    #[test]
    fn mailto_status_bar_and_opener_use_the_same_canonical_target() {
        let raw = "mailto:ada@example.com, bea@example.com?Subject=Hi&body=Text&priority=high";
        let canonical = "mailto:ada@example.com,bea@example.com?subject=Hi&body=Text";
        let check = |label: &str, target: &str| {
            let preview = link_preview(label, target).unwrap();
            assert!(!preview.mismatch);
            let command = open_command(target);
            assert_eq!(command.get_program(), "xdg-open");
            assert!(command.get_args().eq([std::ffi::OsStr::new(canonical)]));
            assert_eq!(preview.target.as_str(), canonical);
        };

        let plain = PlainText::from(raw);
        let [link] = plain.links.as_ref() else {
            panic!("{plain:?}");
        };
        assert_eq!(&plain[link.range.clone()], raw);
        check(&plain[link.range.clone()], &link.target);
        assert!(
            link_preview(
                "mailto:ada@example.com, cyd@example.com?Subject=Hi",
                canonical
            )
            .unwrap()
            .mismatch
        );

        let html = html_to_document(&format!(
            "<a href=\"{}\">Write to us</a>",
            raw.replace('&', "&amp;")
        ));
        let [Block::Paragraph { inlines }] = html.blocks.as_slice() else {
            panic!("{html:?}");
        };
        let [Inline::Link { href, .. }] = inlines.as_slice() else {
            panic!("{inlines:?}");
        };
        check("Write to us", &checked_target(href).unwrap());
    }

    #[test]
    fn mailto_preview_matches_path_recipients_and_flags_other_destinations() {
        for (raw, target, destination, mismatch) in [
            (
                "mailto:ada@example.com?subject=Hi&reply-to=evil@attacker.com",
                "mailto:ada@example.com?subject=Hi",
                "ada@example.com",
                true,
            ),
            (
                "mailto:ada@example.com?subject=See https://user:pass@evil.test",
                "mailto:ada@example.com?subject=See%20https%3A%2F%2Fuser%3Apass%40evil.test",
                "ada@example.com",
                true,
            ),
            (
                "mailto:ada@example.com?subject=https://evil.test",
                "mailto:ada@example.com?subject=https%3A%2F%2Fevil.test",
                "ada@example.com",
                true,
            ),
            (
                "mailto:ada@example.com, bea@example.com?Subject=Hi",
                "mailto:ada@example.com,bea@example.com?subject=Hi",
                "ada@example.com,bea@example.com",
                false,
            ),
            (
                "mailto:ada+tag@example.com,bea@example.com?Subject=Hi",
                "mailto:ada%2Btag@example.com,bea@example.com?subject=Hi",
                "ada%2btag@example.com,bea@example.com",
                false,
            ),
        ] {
            let html = html_to_document(&format!(
                "<a href=\"{source}\">{source}</a>",
                source = raw.replace('&', "&amp;"),
            ));
            let [Block::Paragraph { inlines }] = html.blocks.as_slice() else {
                panic!("{html:?}");
            };
            let [Inline::Link { href, children }] = inlines.as_slice() else {
                panic!("{inlines:?}");
            };
            let [Inline::Text(text)] = children.as_slice() else {
                panic!("{children:?}");
            };
            let target_link = checked_target(href).unwrap();
            assert_eq!(target_link.as_ref(), target);
            let preview = link_preview(text, &target_link).unwrap();
            assert_eq!(preview.destination.as_str(), destination);
            assert_eq!(preview.mismatch, mismatch, "{raw}");
        }

        let target = "mailto:ada@example.com,bea@example.com?subject=Hi";
        let preview = link_preview("ada@example.com", target).unwrap();
        assert_eq!(
            preview.destination.as_str(),
            "ada@example.com,bea@example.com"
        );
        assert!(!preview.mismatch);
        assert!(
            link_preview("Write here? &evil.test", target)
                .unwrap()
                .mismatch
        );
    }

    #[test]
    fn wrapped_outlines_only_draw_exposed_horizontal_segments() {
        let fragment = |left, width| Bounds {
            origin: point(px(left), px(0.)),
            size: size(px(width), px(20.)),
        };
        let segments = |rect: &Bounds<Pixels>, neighbor: Option<&Bounds<Pixels>>| {
            outline_segments(outline_span(rect), neighbor.map(outline_span)).collect::<Vec<_>>()
        };
        let long = fragment(0., 100.);
        let short = fragment(0., 60.);
        let full = px(-2.)..px(102.);
        let overhang = px(62.)..px(102.);

        // Longer first: the first row closes only its overhang at the seam.
        assert_eq!(segments(&long, None), std::slice::from_ref(&full));
        assert_eq!(
            segments(&long, Some(&short)),
            std::slice::from_ref(&overhang)
        );
        assert!(segments(&short, Some(&long)).is_empty());
        assert_eq!(segments(&short, None), [px(-2.)..px(62.)]);

        // Shorter first: the same overhang belongs to the second row's top.
        assert_eq!(segments(&short, None), [px(-2.)..px(62.)]);
        assert!(segments(&short, Some(&long)).is_empty());
        assert_eq!(segments(&long, Some(&short)), [overhang]);
        assert_eq!(segments(&long, None), [full]);

        // Equal widths leave the whole seam open.
        assert!(segments(&long, Some(&long)).is_empty());

        let inset = fragment(20., 60.);
        assert_eq!(
            segments(&long, Some(&inset)),
            [px(-2.)..px(18.), px(82.)..px(102.)]
        );
    }
}
