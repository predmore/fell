//! The wrapping file chips beneath an expanded message.

use fell_core::MimePart;
use gpui::{
    AnyElement, Context, FontWeight, InteractiveElement, IntoElement, MouseButton, ParentElement,
    PathBuilder, Styled, Window, div, point, prelude::*, px, rgb,
};

use super::{ConvFrame, Inbox, LayoutProbe, LayoutSlot, Live, conversation_line_style};
use crate::attachments::{self, Request, Target};
use crate::keymap::{self, Action};
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
    let max_width = width.min(338.0);
    let font_size = (theme.ui_px - 1.0).max(12.0);
    let style = conversation_line_style(window, theme, px(font_size - 1.0));
    let measure =
        |text: &str| super::line_px_with_style(window, &style, text, FontWeight::NORMAL).ceil();
    let retry = format!(
        "Failed · {}",
        keymap::binding_text(&session.keymap, Action::RetryAttachment)
    );
    let status_width = ["Downloading", "↓ 100%", &retry]
        .into_iter()
        .map(measure)
        .fold(0.0, f32::max);
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
                    let key = (message.id.clone(), part.section.clone());
                    let displayed = frame.attachment_sizes.get(&key);
                    let size =
                        attachments::size_label(displayed.map_or(part.size, |size| size.size));
                    let fading = displayed
                        .and_then(|size| size.from)
                        .zip(frame.now)
                        .filter(|((_, fade), now)| fade.running(*now))
                        .map(|((old, fade), now)| {
                            (attachments::size_label(old), fade.progress(now))
                        });
                    let failed = download.is_none() && frame.failed_attachments.contains(&key);
                    let status = if let Some(progress) = download {
                        if progress.total == 0 {
                            "Downloading".to_owned()
                        } else {
                            format!("↓ {}", progress.label())
                        }
                    } else if failed {
                        retry.clone()
                    } else {
                        String::new()
                    };
                    let accent = theme.accent;
                    let icon = file_icon(part, meta);
                    let chip = div()
                        .id(gpui::ElementId::named_usize("attachment", part_index))
                        .relative()
                        .flex()
                        .items_center()
                        .gap(px(8.))
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
                                            if !inbox.search_conversation_live()
                                                || inbox.open_thread != inbox.session.cursor_id
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
                                .relative()
                                .flex_shrink_0()
                                .w(px(measure("999 MB").max(measure(&size))))
                                .text_right()
                                .text_color(rgb(meta))
                                .text_size(px(font_size - 1.0))
                                .whitespace_nowrap()
                                .child(
                                    div()
                                        .opacity(fading.as_ref().map_or(1.0, |(_, alpha)| *alpha))
                                        .child(size),
                                )
                                .children(fading.map(|(old, alpha)| {
                                    div().absolute().inset_0().opacity(1.0 - alpha).child(old)
                                })),
                        )
                        .child(
                            div()
                                .flex_shrink_0()
                                .w(px(status_width))
                                .text_right()
                                .text_color(rgb(if failed { theme.error } else { meta }))
                                .text_size(px(font_size - 1.0))
                                .whitespace_nowrap()
                                .child(status),
                        )
                        .children(download.map(|progress| {
                            let shown = if progress.total == 0 {
                                0.0
                            } else {
                                (progress.fetched as f32 / progress.total as f32).clamp(0.0, 1.0)
                            };
                            gpui::canvas(
                                |_, _, _| (),
                                move |mut bounds, (), window, _| {
                                    bounds.size.width = (bounds.size.width * shown)
                                        .max(px(10.))
                                        .min(bounds.size.width);
                                    window.paint_quad(gpui::fill(bounds, rgb(accent)));
                                },
                            )
                            .absolute()
                            .left(px(2.))
                            .right(px(2.))
                            .bottom_0()
                            .h(px(1.))
                        }))
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
