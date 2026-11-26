use std::time::{Duration, Instant};

use fell_core::{Identity, compose::DraftSession};
use gpui::{
    Context, FontWeight, InteractiveElement, MouseButton, ParentElement, ScrollHandle,
    StatefulInteractiveElement, Styled, Window, div, prelude::*, px, rgb,
};

use super::ComposeView;
use crate::{keymap, motion, present, theme};

pub(super) const KEPT_SIGNATURE: &str = "Kept your edited signature";
pub(super) const SIGNATURE_SEPARATOR: &str = "-- ";
pub(super) const NOTICE_HOLD: Duration = Duration::from_millis(2500);

#[derive(Default)]
pub(super) struct Display {
    current: Sender,
    managed: Option<String>,
    fade: Option<SenderFade>,
    progress: f32,
    kept_since: Option<Instant>,
}

#[derive(Default)]
struct Sender {
    name: Option<String>,
    address: String,
    other_than_default: bool,
}

struct SenderFade {
    from: Sender,
    footer: Option<String>,
    transition: motion::Transition,
}

impl Display {
    pub(super) fn update(
        &mut self,
        from: &str,
        default: bool,
        managed: Option<String>,
        body: &str,
        motion: motion::Motion,
        window: &mut Window,
    ) -> bool {
        let now = Instant::now();
        let (name, address) =
            fell_core::compose::mailbox_parts(from).unwrap_or_else(|| (None, from.to_string()));
        let address = present::display_safe(&address);
        let name = name.map(|name| present::display_safe(&name));
        let changed_address = !self.current.address.is_empty()
            && !self.current.address.eq_ignore_ascii_case(&address);
        let changed = self.current.address != address || self.current.name != name;
        if changed {
            let previous = std::mem::replace(
                &mut self.current,
                Sender {
                    name,
                    address,
                    other_than_default: !default,
                },
            );
            self.fade = motion
                .start(motion::FADE, now)
                .filter(|_| !previous.address.is_empty())
                .map(|transition| {
                    let footer = if managed.is_some() && managed != self.managed {
                        self.managed.take()
                    } else {
                        None
                    };
                    SenderFade {
                        from: previous,
                        footer,
                        transition,
                    }
                });
        }
        self.current.other_than_default = !default;
        self.managed = managed;
        self.fade = self
            .fade
            .take()
            .filter(|fade| motion.enabled() && fade.transition.running(now));
        self.progress = self.fade.as_ref().map_or(1., |fade| {
            window.request_animation_frame();
            fade.transition.progress(now)
        });

        let kept = changed_address
            && self.managed.is_none()
            && body.lines().any(|line| line == SIGNATURE_SEPARATOR);
        if kept {
            self.kept_since = Some(now);
        }
        kept
    }

    pub(super) fn settle(&mut self) {
        self.fade = None;
        self.progress = 1.;
    }

    pub(super) fn kept_opacity(
        &mut self,
        motion: motion::Motion,
        window: &mut Window,
    ) -> Option<f32> {
        let started = self.kept_since?;
        let now = Instant::now();
        if now < started + motion::FADE && motion.enabled() {
            window.request_animation_frame();
            return Some(motion::Transition::new(started, motion::FADE).progress(now));
        }
        let leave = started + NOTICE_HOLD;
        if now < leave {
            return Some(1.);
        }
        if now < leave + motion::FADE && motion.enabled() {
            window.request_animation_frame();
            return Some(motion::Transition::new(leave, motion::FADE).remaining(now));
        }
        self.kept_since = None;
        None
    }

    pub(super) fn footer(&self) -> Option<(&str, f32)> {
        self.fade
            .as_ref()?
            .footer
            .as_deref()
            .map(|footer| (footer, self.progress))
    }

    pub(super) fn label(&self, theme: &theme::Theme) -> gpui::Div {
        div()
            .relative()
            .flex_1()
            .min_w_0()
            .child(sender_label(&self.current, theme).opacity(self.progress))
            .children(self.fade.as_ref().map(|fade| {
                sender_label(&fade.from, theme)
                    .absolute()
                    .inset_0()
                    .opacity(1. - self.progress)
            }))
    }
}

fn sender_label(sender: &Sender, theme: &theme::Theme) -> gpui::Div {
    let dim = theme.dimmed;
    let name_color = if sender.other_than_default {
        theme.foreground
    } else {
        dim
    };
    div()
        .flex()
        .items_baseline()
        .justify_end()
        .min_w_0()
        .gap(px(8.))
        .child(div().flex_none().text_color(rgb(dim)).child("From"))
        .children(sender.name.as_ref().map(|name| {
            div()
                .max_w(gpui::relative(0.45))
                .truncate()
                .font_weight(FontWeight::MEDIUM)
                .text_color(rgb(name_color))
                .child(name.clone())
        }))
        .child(
            div()
                .min_w_0()
                .truncate()
                .text_color(rgb(if sender.name.is_some() {
                    dim
                } else {
                    name_color
                }))
                .child(sender.address.clone()),
        )
}

pub(super) struct Picker {
    choices: Vec<Identity>,
    current: Option<String>,
    received: Option<String>,
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
        let Some((choices, current, received)) = self
            .hub
            .with_session(self.draft_id.as_ref(), |session| {
                let from = &session.fields().from;
                (session.send_lock().is_none() && can_pick(session, from)).then(|| {
                    let choices = session.identities();
                    let received = session.reply_context().and_then(|parent| {
                        parent
                            .arrived_via
                            .iter()
                            .find(|address| {
                                choices
                                    .iter()
                                    .any(|identity| identity.address.eq_ignore_ascii_case(address))
                            })
                            .cloned()
                    });
                    (
                        choices.to_vec(),
                        fell_core::compose::email_of(from),
                        received,
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
            received,
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
        let viewport_width = f32::from(window.viewport_size().width);
        let narrow = viewport_width < 520.;
        let width = (viewport_width - 32.).clamp(0., 460.);
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
                            let received = picker.received.as_ref().is_some_and(|received| {
                                identity.address.eq_ignore_ascii_case(received)
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
                                .when(received, |row| {
                                    row.child(
                                        div()
                                            .flex_none()
                                            .text_size(px(12.))
                                            .text_color(rgb(theme.dimmed))
                                            .child("received here"),
                                    )
                                })
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
                .pt(px(44. - 4. * (1. - progress)))
                .pr(px(if narrow { 0. } else { 16. }))
                .flex()
                .when(narrow, |layer| layer.justify_center())
                .when(!narrow, |layer| layer.justify_end())
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
