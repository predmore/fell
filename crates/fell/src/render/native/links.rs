use std::{
    ops::Range,
    process::{Command, Stdio},
    sync::Arc,
};

use gpui::{
    AnyElement, App, AvailableSpace, Bounds, CursorStyle, ElementId, FocusHandle, MouseButton,
    Pixels, Render, SharedString, Window, div, point, prelude::*, px, size,
};

use super::{MailText, Mark, Palette, SOFT_HYPHEN, ink, paint};

pub(super) struct LinkSpan {
    pub range: Range<usize>,
    pub target: Arc<str>,
}

pub(super) fn link_spans(runs: &[(usize, Mark)]) -> Vec<LinkSpan> {
    let mut links: Vec<LinkSpan> = Vec::new();
    let mut at = 0;
    for (len, mark) in runs {
        let end = at + len;
        if let Some(target) = &mark.link {
            if let Some(last) = links.last_mut()
                && last.range.end == at
                && Arc::ptr_eq(&last.target, target)
            {
                last.range.end = end;
            } else {
                links.push(LinkSpan {
                    range: at..end,
                    target: target.clone(),
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
    handles: Vec<Option<(Arc<str>, FocusHandle)>>,
}

pub(super) fn link_controls(
    mail: &MailText,
    bounds: Bounds<Pixels>,
    focus: &mut LinkFocus,
    window: &mut Window,
    cx: &mut App,
) -> Vec<AnyElement> {
    if focus.text != mail.text {
        focus.text = mail.text.clone();
        focus.handles.clear();
    }
    focus.handles.resize_with(mail.links.len(), || None);
    let mut fragments = vec![Vec::<Bounds<Pixels>>::new(); mail.links.len()];
    let clip = window.content_mask().bounds.intersect(&bounds);
    mail.for_visible_line(bounds, window, |line, shaped, origin, _| {
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
            }
            .intersect(&clip);
            if rect.is_empty() {
                continue;
            }
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

    let mut controls = Vec::new();
    for (index, (link, rects)) in mail.links.iter().zip(fragments).enumerate() {
        let Some(first) = rects.first() else {
            continue;
        };
        let slot = &mut focus.handles[index];
        if slot
            .as_ref()
            .is_some_and(|(target, _)| target != &link.target)
        {
            *slot = None;
        }
        let (_, handle) =
            slot.get_or_insert_with(|| (link.target.clone(), cx.focus_handle().tab_stop(true)));
        let focused = handle.is_focused(window);
        let preview = TargetPreview {
            target: SharedString::from(link.target.to_string()),
            palette: mail.palette,
            max_width: (window.viewport_size().width - px(16.)).min(px(520.)),
        };
        if focused {
            let mut label = preview.label().into_any_element();
            let measured = label.layout_as_root(
                size(AvailableSpace::MaxContent, AvailableSpace::MaxContent),
                window,
                cx,
            );
            let viewport = window.viewport_size();
            let x = first
                .left()
                .min(viewport.width - measured.width - px(8.))
                .max(px(8.));
            let below = rects.last().unwrap_or(first).bottom() + px(4.);
            let y = if below + measured.height <= viewport.height {
                below
            } else {
                (first.top() - measured.height - px(4.)).max(px(0.))
            };
            window.defer_draw(label, point(x, y), 0);
        }
        let target = link.target.clone();
        let mut control = div()
            .relative()
            .size_0()
            .id(ElementId::named_usize("mail-link", index))
            .track_focus(handle)
            .on_key_down(move |event, _, cx| {
                if !event.is_held
                    && event.keystroke.modifiers == gpui::Modifiers::default()
                    && matches!(event.keystroke.key.as_str(), "enter" | "return")
                {
                    open(target.clone());
                    cx.stop_propagation();
                }
            })
            .children(rects.into_iter().enumerate().map(|(at, rect)| {
                let target = link.target.clone();
                let handle = handle.clone();
                let preview = preview.clone();
                div()
                    .absolute()
                    .left(rect.left() - bounds.left())
                    .top(rect.top() - bounds.top())
                    .w(rect.size.width)
                    .h(rect.size.height)
                    .id(ElementId::named_usize("link-fragment", at))
                    .cursor(CursorStyle::PointingHand)
                    .when(focused, |fragment| {
                        fragment
                            .rounded(px(2.))
                            .bg(ink(mail.palette.accent).opacity(0.18))
                    })
                    .when(!focused, |fragment| {
                        fragment.tooltip(move |_, cx| cx.new(|_| preview.clone()).into())
                    })
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(move |_, window, cx| {
                        window.focus(&handle);
                        open(target.clone());
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
        controls.push(control);
    }
    controls
}

#[derive(Clone)]
struct TargetPreview {
    target: SharedString,
    palette: Palette,
    max_width: Pixels,
}

impl TargetPreview {
    fn label(&self) -> gpui::Div {
        div()
            .max_w(self.max_width)
            .overflow_hidden()
            .text_ellipsis()
            .text_size(px(12.))
            .line_height(px(18.))
            .px(px(8.))
            .py(px(4.))
            .rounded(px(4.))
            .border_1()
            .border_color(paint(self.palette.accent))
            .bg(paint(self.palette.background))
            .text_color(paint(self.palette.foreground))
            .child(self.target.clone())
    }
}

impl Render for TargetPreview {
    fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
        self.label()
    }
}

fn open(target: Arc<str>) {
    std::thread::spawn(move || {
        let result = Command::new("xdg-open")
            .arg(target.as_ref())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if !result.is_ok_and(|status| status.success()) {
            fell_core::diagnostic!(WARN, "Could not open link");
        }
    });
}
