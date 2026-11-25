use std::time::Instant;

use fell_core::{Identity, compose::DraftSession};
use gpui::{
    Context, FontWeight, InteractiveElement, MouseButton, ParentElement, ScrollHandle,
    StatefulInteractiveElement, Styled, Window, div, prelude::*, px, rgb,
};

use super::ComposeView;
use crate::{keymap, motion, present, theme};

pub(super) struct Picker {
    choices: Vec<Identity>,
    current: Option<String>,
    query: String,
    selected: usize,
    scroll: ScrollHandle,
    transition: Option<motion::Transition>,
    closing: bool,
}

impl Picker {
    pub(super) fn active(&self) -> bool {
        !self.closing
    }

    fn matches(&self) -> Vec<&Identity> {
        let mut choices: Vec<_> = self
            .choices
            .iter()
            .filter_map(|identity| {
                let label = format!("{} {}", identity.name, identity.address);
                keymap::fuzzy(&self.query, &label).map(|score| (score, identity))
            })
            .collect();
        choices.sort_by(|(left, _), (right, _)| right.cmp(left));
        choices.into_iter().map(|(_, identity)| identity).collect()
    }
}

pub(super) fn can_pick(session: &DraftSession, from: &str) -> bool {
    let identities = session.identities();
    identities.len() > 1
        || identities.first().is_some_and(|default| {
            !fell_core::compose::email_of(from)
                .is_some_and(|address| default.address.eq_ignore_ascii_case(&address))
        })
}

impl ComposeView {
    pub(super) fn open_identity_picker(&mut self) {
        let Some((choices, current)) = self
            .hub
            .with_session(self.draft_id.as_ref(), |session| {
                let from = &session.fields().from;
                (session.send_lock().is_none() && can_pick(session, from)).then(|| {
                    (
                        session.identities().to_vec(),
                        fell_core::compose::email_of(from),
                    )
                })
            })
            .flatten()
        else {
            return;
        };
        let selected = choices
            .iter()
            .position(|identity| {
                current
                    .as_ref()
                    .is_some_and(|current| identity.address.eq_ignore_ascii_case(current))
            })
            .unwrap_or(0);
        let scroll = ScrollHandle::new();
        scroll.scroll_to_item(selected);
        self.dismiss_recipient_menu();
        self.from_picker = Some(Picker {
            choices,
            current,
            selected,
            scroll,
            query: String::new(),
            transition: self.motion.start(motion::FADE, Instant::now()),
            closing: false,
        });
    }

    fn close_identity_picker(&mut self) {
        if let Some(picker) = self.from_picker.as_mut() {
            picker.closing = true;
            picker.transition = self.motion.start(motion::FADE, Instant::now());
        }
    }

    fn pick_identity(&mut self, identity: &Identity, cx: &mut Context<Self>) {
        let result = self.hub.with_session(self.draft_id.as_ref(), |session| {
            let changed = session.set_from_user(identity)?;
            if changed {
                session.arm_outbound(super::now_ms());
            }
            Ok::<_, String>(changed)
        });
        self.close_identity_picker();
        match result {
            Some(Ok(true)) => {
                self.pull_unpainted();
                self.flush_pending = true;
                self.arm_debounced_flush(cx);
            }
            Some(Err(error)) => self.status = error.into(),
            Some(Ok(false)) | None => {}
        }
        cx.notify();
    }

    pub(super) fn identity_key(&mut self, key: &str, typed: Option<&str>, cx: &mut Context<Self>) {
        let Some(picker) = self.from_picker.as_mut().filter(|picker| picker.active()) else {
            return;
        };
        picker.transition = None;
        match key {
            "escape" => self.close_identity_picker(),
            "enter" => {
                if let Some(identity) = picker
                    .matches()
                    .get(picker.selected)
                    .map(|item| (*item).clone())
                {
                    self.pick_identity(&identity, cx);
                }
            }
            "up" | "down" => {
                let len = picker.matches().len();
                if len > 0 {
                    picker.selected =
                        (picker.selected + if key == "up" { len - 1 } else { 1 }) % len;
                    picker.scroll.scroll_to_item(picker.selected);
                }
            }
            _ => {
                if key == "backspace" {
                    picker.query.pop();
                } else if let Some(text) = typed {
                    picker.query.push_str(text);
                } else {
                    return;
                }
                picker.selected = 0;
                picker.scroll.scroll_to_item(0);
            }
        }
    }

    pub(super) fn identity_picker_view(
        &self,
        theme: &theme::Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Div> {
        let picker = self.from_picker.as_ref()?;
        let now = Instant::now();
        let transition = picker.transition.filter(|_| self.motion.enabled());
        let progress = if picker.active() {
            motion::progress(transition.as_ref(), now)
        } else {
            transition.as_ref()?.remaining(now)
        };
        if picker.closing && progress <= 0. {
            return None;
        }
        if progress < 1. || picker.closing {
            window.request_animation_frame();
        }
        let width = (f32::from(window.viewport_size().width) - 24.).clamp(0., 460.);
        let height = f32::from(window.viewport_size().height);
        let matches = picker.matches();
        let card =
            div()
                .w(px(width * motion::lerp(0.96, 1., progress)))
                .max_h(px((height - 92.).clamp(96., 348.)))
                .opacity(progress)
                .flex()
                .flex_col()
                .bg(rgb(theme.surface))
                .border_1()
                .border_color(rgb(theme.border))
                .rounded_md()
                .shadow_lg()
                .overflow_hidden()
                .when(picker.active(), |card| {
                    card.on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                })
                .child(
                    div()
                        .flex_none()
                        .h(px(present::CARD_QUERY_PX))
                        .px(px(14.))
                        .flex()
                        .items_center()
                        .gap_2()
                        .border_b_1()
                        .border_color(rgb(theme.border))
                        .child(
                            div()
                                .flex_none()
                                .text_color(rgb(theme.accent))
                                .child("From"),
                        )
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .child(present::display_safe(&picker.query)),
                        )
                        .child(div().flex_none().w(px(2.)).h(px(16.)).bg(rgb(theme.accent))),
                )
                .child(
                    div()
                        .id("compose-identity-rows")
                        .min_h_0()
                        .overflow_y_scroll()
                        .track_scroll(&picker.scroll)
                        .py_1()
                        .when(matches.is_empty(), |rows| {
                            rows.child(
                                div()
                                    .px(px(14.))
                                    .py_2()
                                    .text_color(rgb(theme.dimmed))
                                    .child("No identities match"),
                            )
                        })
                        .children(matches.into_iter().enumerate().map(|(index, identity)| {
                            let selected = index == picker.selected;
                            let current = picker.current.as_ref().is_some_and(|current| {
                                identity.address.eq_ignore_ascii_case(current)
                            });
                            let name = present::display_safe(&identity.name);
                            let address = present::display_safe(&identity.address);
                            let option = identity.clone();
                            div()
                                .id(gpui::ElementId::named_usize("compose-identity", index))
                                .h(px(42.))
                                .mx_1()
                                .px(px(10.))
                                .rounded_md()
                                .flex()
                                .items_center()
                                .gap_3()
                                .when(selected, |row| {
                                    row.bg(rgb(theme.selection))
                                        .text_color(rgb(theme.on_selection))
                                })
                                .when(picker.active(), |row| {
                                    row.cursor_pointer().on_click(cx.listener(
                                        move |this, _, _, cx| this.pick_identity(&option, cx),
                                    ))
                                })
                                .child(
                                    div()
                                        .min_w_0()
                                        .flex_1()
                                        .flex()
                                        .items_baseline()
                                        .gap_2()
                                        .when(!name.is_empty(), |line| {
                                            line.child(
                                                div()
                                                    .max_w(gpui::relative(0.45))
                                                    .truncate()
                                                    .font_weight(FontWeight(550.))
                                                    .child(name),
                                            )
                                        })
                                        .child(
                                            div()
                                                .min_w_0()
                                                .truncate()
                                                .when(!identity.name.is_empty(), |line| {
                                                    line.text_color(rgb(if selected {
                                                        theme.on_selection_dim
                                                    } else {
                                                        theme.dimmed
                                                    }))
                                                })
                                                .child(address),
                                        ),
                                )
                                .when(current, |row| row.child(div().flex_none().child("✓")))
                        })),
                );
        Some(
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .left_0()
                .right_0()
                .pt(px(68. - 4. * (1. - progress)))
                .flex()
                .justify_center()
                .items_start()
                .when(picker.active(), |layer| {
                    layer.occlude().on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            this.close_identity_picker();
                            cx.notify();
                        }),
                    )
                })
                .child(card),
        )
    }
}
