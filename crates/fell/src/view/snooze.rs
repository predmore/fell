use fell_core::store::{MUTED, SNOOZED};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    Context, FontWeight, InteractiveElement, MouseButton, ParentElement, Styled, Window, div, px,
    rgb,
};

use crate::model::Overlay;
use crate::model::snooze::{PRESETS, Picker, TriageEdit, TriageKind, preset, typed};

use super::{Inbox, ReadIntent, Theme, ThreadId, motion, now_unix, present};

impl Inbox {
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
                    .map(|_| {
                        format!(
                            "Snoozed until {}",
                            present::activity_time(now_unix(), until)
                        )
                    }),
                TriageKind::Unsnooze => {
                    store
                        .set_thread_snoozes(&account, &targets, None)
                        .map(|count| {
                            if count > 0 {
                                "Unsnoozed"
                            } else {
                                "No snoozes to clear"
                            }
                            .into()
                        })
                }
                TriageKind::Mute => store
                    .toggle_thread_mutes(&account, &targets)
                    .map(|mute| if mute { "Muted" } else { "Unmuted" }.into()),
            },
            move |inbox, result, cx| {
                if removed {
                    for id in ids {
                        inbox.pending_removals.remove(&id);
                    }
                }
                let failed = result.is_err();
                let text = result.unwrap_or_else(|err| err.to_string());
                inbox.session.show_toast(text, failed);
                inbox.expire_action_notice(cx);
                Some(ReadIntent {
                    changed: true,
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
        if self.in_inbox_list() || self.list_folder == SNOOZED {
            self.request_threads(
                ReadIntent {
                    refresh_open: true,
                    ..ReadIntent::default()
                },
                cx,
            );
        }
    }
}

pub(super) fn card(
    theme: &Theme,
    picker: &Picker,
    opacity: f32,
    window: &Window,
    cx: &mut Context<Inbox>,
) -> gpui::Div {
    let width = (f32::from(window.viewport_size().width) - 24.0).clamp(1.0, 380.0);
    let height = 210.0;
    let scale = motion::lerp(0.97, 1.0, opacity);
    let now = picker.opened_at;
    let query = picker.query.trim();
    let custom = (!query.is_empty()).then(|| typed(now, query)).flatten();
    let detail_color = if picker.error {
        theme.error
    } else {
        theme.dimmed
    };
    let detail = if picker.error {
        "Enter a time like mon 9am or 18:30".into()
    } else if let Some(at) = custom {
        format!("Wake {}", present::activity_time(now, at))
    } else {
        String::new()
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
        .justify_center()
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
                .h(px(height - 2.0))
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
                        .child("Snooze until"),
                )
                .child(
                    div()
                        .h(px(36.0))
                        .px(px(16.0))
                        .flex()
                        .items_center()
                        .border_b_1()
                        .border_color(rgb(theme.border))
                        .text_color(rgb(if query.is_empty() {
                            theme.dimmed
                        } else {
                            theme.foreground
                        }))
                        .child(if query.is_empty() {
                            "Type a time, e.g. mon 9am".into()
                        } else {
                            picker.query.clone()
                        }),
                )
                .child(
                    div()
                        .pt(px(4.0))
                        .px(px(5.0))
                        .children(PRESETS.iter().enumerate().map(|(index, label)| {
                            let selected = query.is_empty() && index == picker.index;
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
                                        .child(preset(now, index).map_or_else(String::new, |at| {
                                            present::activity_time(now, at)
                                        })),
                                )
                        })),
                )
                .child(
                    div()
                        .h(px(29.0))
                        .px(px(16.0))
                        .flex()
                        .items_center()
                        .text_size(px(theme.ui_px - 1.0))
                        .text_color(rgb(detail_color))
                        .child(detail),
                ),
        )
}
