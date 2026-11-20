//! The wrapping file chips beneath an expanded message.

use fell_core::MimePart;
use gpui::{
    AnyElement, Context, FontWeight, InteractiveElement, IntoElement, MouseButton, ParentElement,
    PathBuilder, Styled, Window, div, point, prelude::*, px, rgb,
};

use super::{ConvFrame, Inbox, LayoutProbe, LayoutSlot, Live, conversation_line_style};
use crate::attachments::{self, Request, Target};
use crate::model::Session;
use crate::present::ConvMessage;

use crate::theme::Theme;

#[allow(clippy::too_many_arguments)]
pub(super) fn row(
    theme: &Theme,
    session: &Session,
    message: &ConvMessage,
    index: usize,
    width: f32,
    live: Live,
    frame: &ConvFrame,
    probe: Option<&LayoutSlot>,
    window: &Window,
    cx: &mut Context<Inbox>,
) -> gpui::Div {
    let selected = session
        .focused_attachment()
        .filter(|target| target.message == message.id);
    let max_width = width.min(300.0);
    let font_size = (theme.ui_px - 1.0).max(12.0);
    let style = conversation_line_style(window, theme, px(font_size - 1.0));
    let measure =
        |text: &str| super::line_px_with_style(window, &style, text, FontWeight::NORMAL).ceil();
    div()
        .mt(px(16.))
        .flex()
        .flex_wrap()
        .gap(px(7.))
        .text_size(px(font_size))
        .children(
            message
                .attachments
                .iter()
                .enumerate()
                .map(|(part_index, part)| {
                    let focused = selected.is_some_and(|target| &target.part == part);
                    let (surface, foreground, meta, border) = if focused {
                        (
                            theme.selection,
                            theme.on_selection,
                            theme.on_selection_dim,
                            theme.accent,
                        )
                    } else {
                        (
                            theme.background,
                            theme.foreground,
                            theme.dimmed,
                            theme.border,
                        )
                    };
                    let download =
                        frame
                            .attachment_download
                            .as_ref()
                            .and_then(|(id, section, progress)| {
                                (*id == message.id && *section == part.section).then_some(*progress)
                            });
                    let size = attachments::size_label(part.size);
                    let meta_width = measure(&size).max(measure("100%"));
                    let icon = match download {
                        Some(progress) => {
                            let tick = if frame.spin_attachment {
                                progress.started.elapsed().as_millis() / 80
                            } else {
                                0
                            };
                            spinner(meta, tick as usize)
                        }
                        None => file_icon(part, meta),
                    };
                    let chip = div()
                        .id(gpui::ElementId::named_usize("attachment", part_index))
                        .flex()
                        .items_center()
                        .gap(px(7.))
                        .h(px(32.))
                        .max_w(px(max_width))
                        .px(px(8.))
                        .border_1()
                        .rounded_md()
                        .border_color(rgb(border))
                        .bg(rgb(surface))
                        .text_color(rgb(foreground))
                        .when(live.0, |chip| {
                            let part = part.clone();
                            let id = message.id.clone();
                            chip.cursor_pointer()
                                .when(!focused, |chip| {
                                    chip.hover(|style| style.border_color(rgb(theme.accent)))
                                })
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(
                                        move |inbox, event: &gpui::MouseDownEvent, window, cx| {
                                            cx.stop_propagation();
                                            if inbox.open_thread != inbox.session.cursor_id
                                                || !inbox.session.messages.get(index).is_some_and(
                                                    |message| {
                                                        message.id == id
                                                            && message.expanded
                                                            && message.attachments.contains(&part)
                                                    },
                                                )
                                            {
                                                return;
                                            }
                                            window.focus(&inbox.focus);
                                            inbox.cancel_pending_keys(cx);
                                            inbox.session.clear_toast();
                                            inbox.session.focus_attachment(index, part.clone());
                                            inbox.reveal_selected_attachment();
                                            if event.click_count == 2 {
                                                inbox.start_attachment_action(
                                                    Request::Open(Target {
                                                        message: id.clone(),
                                                        part: part.clone(),
                                                    }),
                                                    false,
                                                    cx,
                                                );
                                            }
                                            cx.notify();
                                        },
                                    ),
                                )
                        })
                        .child(icon)
                        .child(
                            div()
                                .min_w(px(0.))
                                .truncate()
                                .child(attachments::name(part)),
                        )
                        .child(
                            div()
                                .flex_shrink_0()
                                .w(px(meta_width))
                                .text_right()
                                .text_color(rgb(meta))
                                .text_size(px(font_size - 1.0))
                                .whitespace_nowrap()
                                .child(download.map_or(size, |progress| progress.label())),
                        )
                        .into_any_element();
                    match probe.filter(|_| focused) {
                        Some(slot) => LayoutProbe {
                            inner: chip,
                            slot: slot.clone(),
                        }
                        .into_any_element(),
                        None => chip,
                    }
                }),
        )
}

fn file_icon(part: &MimePart, color: u32) -> AnyElement {
    let mime = part.mime_type.as_str();
    let strokes: &[&[(f32, f32)]] = if mime.starts_with("image/") {
        &[
            &[(1., 2.), (13., 2.), (13., 12.), (1., 12.), (1., 2.)],
            &[(1., 10.), (5., 6.), (8., 9.), (10., 7.), (13., 10.)],
        ]
    } else if mime.starts_with("video/") {
        &[
            &[(1., 2.), (13., 2.), (13., 12.), (1., 12.), (1., 2.)],
            &[(6., 4.5), (10., 7.), (6., 9.5), (6., 4.5)],
        ]
    } else if mime.starts_with("audio/") {
        &[
            &[(2., 8.), (2., 6.)],
            &[(4.5, 10.), (4.5, 4.)],
            &[(7., 12.), (7., 2.)],
            &[(9.5, 10.), (9.5, 4.)],
            &[(12., 8.), (12., 6.)],
        ]
    } else if mime == "message/rfc822" {
        &[&[
            (1., 3.),
            (13., 3.),
            (13., 11.),
            (1., 11.),
            (1., 3.),
            (7., 8.),
            (13., 3.),
        ]]
    } else if mime.contains("zip") || mime.contains("tar") || mime.contains("compressed") {
        &[
            &[
                (2., 1.),
                (9., 1.),
                (12., 4.),
                (12., 13.),
                (2., 13.),
                (2., 1.),
            ],
            &[
                (6., 1.),
                (6., 3.),
                (8., 3.),
                (8., 5.),
                (6., 5.),
                (6., 7.),
                (8., 7.),
                (8., 9.),
            ],
        ]
    } else {
        &[
            &[
                (2., 1.),
                (9., 1.),
                (12., 4.),
                (12., 13.),
                (2., 13.),
                (2., 1.),
            ],
            &[(9., 1.), (9., 4.), (12., 4.)],
            &[(4.5, 7.), (9.5, 7.)],
            &[(4.5, 10.), (9.5, 10.)],
        ]
    };
    gpui::canvas(
        |_, _, _| (),
        move |bounds, (), window, _| {
            let origin = bounds.origin;
            let mut path = PathBuilder::stroke(px(1.1));
            for stroke in strokes {
                let ((x, y), rest) = stroke.split_first().expect("icon stroke");
                path.move_to(origin + point(px(*x), px(*y)));
                for &(x, y) in rest {
                    path.line_to(origin + point(px(x), px(y)));
                }
            }
            if let Ok(path) = path.build() {
                window.paint_path(path, rgb(color));
            }
        },
    )
    .size(px(14.))
    .flex_shrink_0()
    .into_any_element()
}

fn spinner(color: u32, tick: usize) -> AnyElement {
    gpui::canvas(
        |_, _, _| (),
        move |bounds, (), window, _| {
            let origin = bounds.origin + point(px(7.), px(7.));
            for i in 0..8 {
                let a = i as f32 * std::f32::consts::TAU / 8.0;
                let mut path = PathBuilder::stroke(px(1.3));
                path.move_to(origin + point(px(a.cos() * 3.4), px(a.sin() * 3.4)));
                path.line_to(origin + point(px(a.cos() * 5.7), px(a.sin() * 5.7)));
                if let Ok(path) = path.build() {
                    let opacity = 0.20 + 0.80 * ((8 + i - tick % 8) % 8) as f32 / 7.0;
                    window.paint_path(path, gpui::Hsla::from(rgb(color)).alpha(opacity));
                }
            }
        },
    )
    .size(px(14.))
    .flex_shrink_0()
    .into_any_element()
}
