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

struct LinkFragment {
    bounds: Bounds<Pixels>,
    starts_line: bool,
    ends_line: bool,
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
    let mut fragments: Vec<Vec<LinkFragment>> = (0..mail.links.len()).map(|_| Vec::new()).collect();
    let clip = window.content_mask().bounds.intersect(&bounds);
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
            let Some(link) = links
                .get(relative)
                .filter(|link| link.range.contains(&byte))
            else {
                continue;
            };
            let right = glyphs.peek().map_or(layout.width, |next| next.position.x);
            if right <= glyph.position.x {
                continue;
            }
            let rect = Bounds {
                origin: point(origin.x + glyph.position.x, origin.y),
                size: size(right - glyph.position.x, mail.line_height),
            }
            .intersect(&clip);
            if rect.is_empty() {
                continue;
            }
            let rectangles = &mut fragments[first + relative];
            if let Some(previous) = rectangles.last_mut()
                && previous.bounds.origin.y == rect.origin.y
                && previous.bounds.right() == rect.left()
            {
                previous.bounds.size.width = rect.right() - previous.bounds.left();
            } else {
                rectangles.push(LinkFragment {
                    bounds: rect,
                    starts_line: link.range.start >= line.range.start,
                    ends_line: link.range.end <= line.range.end,
                });
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
            let preview = if label == link.target.as_ref() {
                None
            } else {
                crate::compose_ui::reader_link_destination(label, &link.target).map(
                    |(destination, mismatch)| LinkPreview {
                        destination: destination.into(),
                        target: link.target.to_string().into(),
                        mismatch,
                    },
                )
            };
            let handle = cx.focus_handle().tab_stop(true);
            let place = Rc::new(Placement {
                line: Cell::default(),
                visible: Cell::default(),
                target: link.target.clone(),
                preview,
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
        state.place.visible.set(!rects.is_empty());
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
            .children(rects.into_iter().enumerate().map(|(at, fragment)| {
                let target = link.target.clone();
                let handle = handle.clone();
                let hover = handle.clone();
                let rect = fragment.bounds;
                div()
                    .absolute()
                    .left(rect.left() - bounds.left())
                    .top(rect.top() - bounds.top())
                    .w(rect.size.width)
                    .h(rect.size.height)
                    .id(ElementId::named_usize("link-fragment", at))
                    .cursor(CursorStyle::PointingHand)
                    .when(focused, |hit| {
                        hit.child(
                            div()
                                .absolute()
                                .top_0()
                                .bottom_0()
                                .left(px(-2.))
                                .right(px(-2.))
                                .border_t(px(1.))
                                .border_color(paint(mail.palette.accent))
                                .when(fragment.starts_line, |outline| {
                                    outline
                                        .border_l(px(1.))
                                        .rounded_tl(px(2.))
                                        .rounded_bl(px(2.))
                                })
                                .when(fragment.ends_line, |outline| {
                                    outline
                                        .border_b(px(1.))
                                        .border_r(px(1.))
                                        .rounded_tr(px(2.))
                                        .rounded_br(px(2.))
                                }),
                        )
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
            }))
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

fn open(target: Arc<str>, window: &Window, cx: &mut App) {
    let task = cx.background_executor().spawn(async move {
        Command::new("xdg-open")
            .arg(target.as_ref())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
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
