use fell_core::store::{MUTED, SNOOZED};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    Context, FontWeight, InteractiveElement, MouseButton, ParentElement, Styled, div, px, rgb,
};

use crate::model::snooze::{
    PRESETS, Picker, TriageEdit, TriageKind, preset, toast_time, wake_time,
};
use crate::model::{Overlay, Toast};

use super::{Inbox, ReadIntent, Theme, ThreadId, motion, now_unix, palette_caret};

impl Inbox {
    pub(super) fn watch_snooze_clock(cx: &mut Context<Self>) -> gpui::Task<()> {
        cx.spawn(async move |this, cx| {
            loop {
                let seconds = 60 - now_unix().rem_euclid(60) as u64;
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(seconds))
                    .await;
                let open = this.update(cx, |inbox, cx| {
                    if inbox.session.overlay == Overlay::Snooze {
                        cx.notify();
                        true
                    } else {
                        false
                    }
                });
                if !open.unwrap_or(false) {
                    break;
                }
            }
        })
    }

    pub(super) fn apply_triage(&mut self, edit: TriageEdit, cx: &mut Context<Self>) {
        let key = self.selected_key();
        let account = self.account_id.clone();
        let ids = edit.ids;
        let targets = ids.iter().cloned().map(ThreadId).collect::<Vec<_>>();
        let removed = self.session.search.is_none()
            && self.list_label.is_none()
            && match edit.kind {
                TriageKind::Snooze(_) => self.in_inbox_list(),
                TriageKind::Unsnooze => self.list_folder == SNOOZED,
                TriageKind::Mute => self.in_inbox_list() || self.list_folder == MUTED,
            };
        self.session.picked.clear();
        self.session.visual = false;
        self.session.anchor_id = None;
        self.session.cancel_read_delay();
        if removed {
            self.take_out_of_list(&ids, cx);
        }
        self.change_mailbox(
            move |store| match edit.kind {
                TriageKind::Snooze(until) => store
                    .set_thread_snoozes(&account, &targets, Some(until))
                    .map(|count| {
                        let at = toast_time(now_unix(), until);
                        match count {
                            0 => ("Snooze unchanged".into(), false),
                            1 => (format!("Snoozed until {at}"), true),
                            n => (format!("Snoozed {n} until {at}"), true),
                        }
                    }),
                TriageKind::Unsnooze => {
                    store
                        .set_thread_snoozes(&account, &targets, None)
                        .map(|count| {
                            if count > 0 {
                                ("Unsnoozed".into(), true)
                            } else {
                                ("No snoozes to clear".into(), false)
                            }
                        })
                }
                TriageKind::Mute => store
                    .toggle_thread_mutes(&account, &targets)
                    .map(|mute| (if mute { "Muted" } else { "Unmuted" }.into(), true)),
            },
            move |inbox, result, cx| {
                if removed {
                    for id in ids {
                        inbox.pending_removals.remove(&id);
                    }
                }
                let failed = result.is_err();
                match result {
                    Ok((text, true)) => inbox.session.set_toast(Toast::with_hint(
                        text,
                        crate::keymap::binding_text(
                            &inbox.session.keymap,
                            crate::keymap::Action::Undo,
                        ),
                        "",
                    )),
                    Ok((text, false)) => inbox.session.show_toast(text, false),
                    Err(err) => inbox.session.show_toast(err.to_string(), true),
                }
                inbox.expire_action_notice(cx);
                Some(ReadIntent {
                    changed: true,
                    refresh_open: true,
                    select: key.filter(|_| removed && failed),
                    ..ReadIntent::default()
                })
            },
            cx,
        );
    }

    fn in_inbox_list(&self) -> bool {
        self.session
            .roles
            .is(fell_core::FolderRole::Inbox, &self.list_folder)
    }

    pub(super) fn snoozes_changed(&mut self, cx: &mut Context<Self>) {
        if let Some(queue) = &self.notifications {
            queue.refresh();
        }
        if self.session.search.is_some() {
            self.refresh_search(cx);
        }
        self.request_threads(
            ReadIntent {
                refresh_open: true,
                ..ReadIntent::default()
            },
            cx,
        );
    }
}

const BASE_HEIGHT: f32 = 188.0;
const ERROR_HEIGHT: f32 = 29.0;

pub(super) fn card_height(error: bool) -> f32 {
    BASE_HEIGHT + if error { ERROR_HEIGHT } else { 0.0 }
}

pub(super) fn card(
    theme: &Theme,
    picker: &Picker,
    opacity: f32,
    height: Option<f32>,
    available: f32,
    cx: &mut Context<Inbox>,
) -> gpui::Div {
    let width = (available - 24.0).clamp(1.0, 416.0);
    let height = height.unwrap_or_else(|| card_height(picker.error));
    let scale = motion::lerp(0.97, 1.0, opacity);
    let now = now_unix();
    let typing = !picker.query.is_empty();
    let detail = (typing && !picker.error).then(|| {
        picker
            .deadline(now)
            .map_or_else(|| "Keep typing a time".into(), |at| wake_time(now, at))
    });
    let title = if picker.selected_count > 1 {
        format!("Snooze {} conversations until", picker.selected_count)
    } else {
        "Snooze until".into()
    };

    div()
        .w(px(width * scale))
        .h(px(height * scale))
        .overflow_hidden()
        .bg(rgb(theme.surface))
        .border_1()
        .border_color(rgb(theme.border))
        .rounded_md()
        .shadow_lg()
        .opacity(opacity)
        .flex()
        .justify_start()
        .items_center()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|inbox, _, _, cx| {
                if inbox.session.visible_overlay() == Overlay::Snooze {
                    cx.stop_propagation();
                }
            }),
        )
        .child(
            div()
                .w(px(width - 2.0))
                .flex_none()
                .flex()
                .flex_col()
                .child(
                    div()
                        .h(px(42.0))
                        .px(px(16.0))
                        .flex()
                        .items_center()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(title),
                )
                .child(
                    div()
                        .h(px(36.0))
                        .px(px(16.0))
                        .flex()
                        .items_center()
                        .border_b_1()
                        .border_color(rgb(theme.border))
                        .gap(px(10.0))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .items_center()
                                .gap(px(4.0))
                                .when(!typing, |input| input.child(palette_caret(theme)))
                                .child(
                                    div()
                                        .min_w_0()
                                        .truncate()
                                        .text_color(rgb(if typing {
                                            theme.foreground
                                        } else {
                                            theme.dimmed
                                        }))
                                        .child(if typing {
                                            picker.query.clone()
                                        } else {
                                            "Type a time, e.g. mon 9am".into()
                                        }),
                                )
                                .when(typing, |input| input.child(palette_caret(theme))),
                        )
                        .children(detail.map(|detail| {
                            div()
                                .max_w(px(width * 0.52))
                                .truncate()
                                .text_size(px(theme.ui_px - 1.0))
                                .text_color(rgb(theme.dimmed))
                                .child(detail)
                        })),
                )
                .child(
                    div()
                        .py(px(6.0))
                        .px(px(5.0))
                        .children(PRESETS.iter().enumerate().map(|(index, label)| {
                            let selected = !typing && index == picker.index;
                            div()
                                .h(px(32.0))
                                .px(px(10.0))
                                .rounded_md()
                                .flex()
                                .items_center()
                                .justify_between()
                                .gap(px(8.0))
                                .when(selected, |row| row.bg(rgb(theme.selection)))
                                .text_color(rgb(if selected {
                                    theme.on_selection
                                } else if typing {
                                    theme.dimmed
                                } else {
                                    theme.foreground
                                }))
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |inbox, _, window, cx| {
                                        if inbox.session.visible_overlay() != Overlay::Snooze {
                                            return;
                                        }
                                        cx.stop_propagation();
                                        window.focus(&inbox.focus);
                                        let before = inbox.before_step();
                                        inbox.session.snooze.query.clear();
                                        inbox.session.snooze.index = index;
                                        let step = inbox.session.confirm_snooze();
                                        inbox.apply_step(step, before, cx);
                                    }),
                                )
                                .child(*label)
                                .child(
                                    div()
                                        .flex_none()
                                        .text_size(px(theme.ui_px - 1.0))
                                        .text_color(rgb(if selected {
                                            theme.on_selection_dim
                                        } else {
                                            theme.dimmed
                                        }))
                                        .child(
                                            preset(now, index)
                                                .map_or_else(String::new, |at| wake_time(now, at)),
                                        ),
                                )
                        })),
                )
                .when(picker.error, |card| {
                    card.child(
                        div()
                            .h(px(ERROR_HEIGHT))
                            .px(px(16.0))
                            .flex()
                            .items_center()
                            .text_size(px(theme.ui_px - 1.0))
                            .text_color(rgb(theme.error))
                            .child("Enter a time like mon 9am or 30m"),
                    )
                }),
        )
}
