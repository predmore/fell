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
    let mismatch = mismatch
        && !(target.starts_with("mailto:") && checked_target(label).as_deref() == Some(target));
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
    let mask = window.content_mask().bounds;
    let clip = mask.intersect(&bounds);
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
        let slot = &mut focus.handles[index];
        if slot
            .as_ref()
            .is_some_and(|state| state.target != link.target)
        {
            *slot = None;
        }
        let state = slot.get_or_insert_with(|| {
            let label = mail.text[link.range.clone()].trim();
            let handle = cx.focus_handle().tab_stop(true);
            let place = Rc::new(Placement {
                line: Cell::default(),
                visible: Cell::default(),
                target: link.target.clone(),
                preview: link_preview(label, &link.target),
            });
            reader_links::register(window, cx, &handle, &place);
            LinkState {
                target: link.target.clone(),
                handle,
                place,
            }
        });
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
        let visible = |rect: &Bounds<Pixels>| !rect.intersect(&clip).is_empty();
        state.place.visible.set(rects.iter().any(visible));
        let handle = &state.handle;
        let focused = handle.is_focused(window);
        if focused {
            controls.focused = Some(link.range.clone());
        }
        let target = link.target.clone();
        let mut control = div()
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
                        let target = link.target.clone();
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
                                    &rects,
                                    at,
                                    mask.left()..mask.right(),
                                    paint(mail.palette.accent),
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
            .into_any_element();
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
