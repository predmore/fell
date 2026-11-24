//! The wrapping file chips beneath an expanded message.

use std::collections::HashMap;
use std::time::Instant;

use fell_core::MimePart;
use gpui::{
    AnyElement, Context, FontWeight, InteractiveElement, IntoElement, MouseButton, ParentElement,
    PathBuilder, Styled, Window, div, point, prelude::*, px, rgb,
};

use super::attachment_actions::FailedAction;
use super::{ConvFrame, Inbox, LayoutProbe, LayoutSlot, Live, conversation_line_style};
use crate::attachments::{self, Download, Request, Target};
use crate::model::Session;
use crate::motion::{self, Motion, Transition};
use crate::present::ConvMessage;

use crate::theme::Theme;

type Key = (String, String);

#[derive(Clone, Copy, Default)]
enum Metadata {
    #[default]
    Size,
    Download(Download),
    Failed,
}

#[derive(Default)]
pub(super) struct MetadataMotion(HashMap<Key, MetadataFade>);

#[derive(Default)]
struct MetadataFade {
    current: Metadata,
    change: Option<(Metadata, Transition)>,
}

#[derive(Clone, Copy)]
pub(super) struct MetadataFrame {
    layers: [Option<(Metadata, f32)>; 2],
    pub(super) running: bool,
}

impl MetadataMotion {
    pub(super) fn frame(
        &mut self,
        download: Option<&(String, String, Download)>,
        failed: &HashMap<Key, FailedAction>,
        motion: Motion,
        now: Instant,
    ) -> HashMap<Key, MetadataFrame> {
        let download =
            download.map(|(id, section, progress)| ((id.clone(), section.clone()), *progress));
        for key in failed.keys().chain(download.iter().map(|(key, _)| key)) {
            self.0.entry(key.clone()).or_default();
        }
        let mut frames = HashMap::new();
        self.0.retain(|key, state| {
            let current = match download.as_ref() {
                Some((downloading, progress)) if key == downloading => {
                    Metadata::Download(*progress)
                }
                _ if failed.contains_key(key) => Metadata::Failed,
                _ => Metadata::Size,
            };
            if std::mem::discriminant(&state.current) != std::mem::discriminant(&current) {
                state.change = motion
                    .start(motion::FADE, now)
                    .map(|fade| (state.current, fade));
            }
            state.current = current;
            state.change = state.change.filter(|(_, fade)| fade.running(now));
            let alpha = state.change.map_or(1., |(_, fade)| fade.progress(now));
            let running = state.change.is_some();
            if !running && matches!(current, Metadata::Size) {
                return false;
            }
            frames.insert(
                key.clone(),
                MetadataFrame {
                    layers: [
                        state.change.map(|(old, _)| (old, 1. - alpha)),
                        Some((current, alpha)),
                    ],
                    running,
                },
            );
            true
        });
        frames
    }
}

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
    let name_style = conversation_line_style(window, theme, px(font_size));
    let style = conversation_line_style(window, theme, px(font_size - 1.0));
    let measure =
        |text: &str| super::line_px_with_style(window, &style, text, FontWeight::NORMAL).ceil();
    // Size and status take turns in one column. Their content never changes
    // the width available to the filename or moves a chip onto another row.
    let metadata_width = ["999.9 MiB", "Downloading", "↓ 100%", "Failed"]
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
                    let layers = frame
                        .attachment_metadata
                        .get(&key)
                        .map_or([None, Some((Metadata::Size, 1.))], |metadata| {
                            metadata.layers
                        });
                    // Two borders, two pads, two gaps, and the file icon.
                    let name_room = (max_width - metadata_width - 48.).max(0.);
                    let name =
                        fit_filename(&attachments::name(part), name_room, &name_style, window);
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
                        .child(div().min_w(px(0.)).whitespace_nowrap().child(name))
                        .child(
                            div()
                                .relative()
                                .flex_shrink_0()
                                .w(px(metadata_width))
                                .h(px(font_size * 1.3))
                                .text_right()
                                .text_color(rgb(meta))
                                .text_size(px(font_size - 1.0))
                                .whitespace_nowrap()
                                .children(layers.into_iter().flatten().map(|(metadata, alpha)| {
                                    let layer = div().absolute().inset_0().opacity(alpha);
                                    match metadata {
                                        Metadata::Size => layer
                                            .child(
                                                div()
                                                    .opacity(
                                                        fading
                                                            .as_ref()
                                                            .map_or(1., |(_, alpha)| *alpha),
                                                    )
                                                    .child(size.clone()),
                                            )
                                            .children(fading.as_ref().map(|(old, alpha)| {
                                                div()
                                                    .absolute()
                                                    .inset_0()
                                                    .opacity(1. - alpha)
                                                    .child(old.clone())
                                            })),
                                        Metadata::Download(progress) if progress.total == 0 => {
                                            layer.child("Downloading")
                                        }
                                        Metadata::Download(progress) => {
                                            layer.child(format!("↓ {}", progress.label()))
                                        }
                                        Metadata::Failed => {
                                            layer.text_color(rgb(theme.error)).child("Failed")
                                        }
                                    }
                                })),
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

fn fit_filename(name: &str, width: f32, style: &gpui::TextStyle, window: &Window) -> String {
    let fit = |text: &str, width| super::clip_line(window, style, text, width, FontWeight::NORMAL);
    let measure = |text: &str| super::line_px_with_style(window, style, text, FontWeight::NORMAL);
    let clipped = fit(name, width);
    if clipped != name
        && let Some((stem, extension)) = name.rsplit_once('.')
    {
        let tail = format!(".{extension}");
        let stem = fit(stem, (width - measure(&tail)).max(0.));
        let kept = format!("{stem}{tail}");
        if stem.ends_with('…') && stem != "…" && measure(&kept) <= width {
            return kept;
        }
    }
    clipped
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
