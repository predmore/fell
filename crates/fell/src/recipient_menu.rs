use std::future::Future;
use std::pin::pin;
use std::time::{Duration, Instant};

use fell_core::Address;
use futures::future::{Either, select};
use gpui::{Div, FontWeight, Styled, Window, div, prelude::*, px, relative, rgb};

use crate::motion::{self, Glide, Motion, Transition};
use crate::present::display_safe;
use crate::theme::Theme;

pub(crate) const QUERY_DELAY: Duration = Duration::from_millis(16);
pub(crate) const LOADING_DELAY: Duration = Duration::from_millis(150);
pub(crate) const TEXT_INSET: f32 = 8.;
const ROW: f32 = 31.;
const FRAME: f32 = 10.; // top and bottom padding and border

/// A fast store reply needs no loading panel. Prefer the reply if both are ready.
pub(crate) async fn wait_for_lookup<T>(
    reply: impl Future<Output = T>,
    timer: impl Future<Output = ()>,
    on_slow: impl FnOnce(),
) -> T {
    match select(pin!(reply), pin!(timer)).await {
        Either::Left((reply, _)) => reply,
        Either::Right(((), reply)) => {
            on_slow();
            reply.await
        }
    }
}

#[derive(Default)]
struct RowAnimation {
    arrival: Option<Transition>,
    position: Option<Glide>,
}

impl RowAnimation {
    fn top(&self, index: usize, now: Instant) -> f32 {
        self.position
            .as_ref()
            .map_or(index as f32 * ROW, |glide| glide.value(now))
    }

    fn opacity(&self, now: Instant) -> f32 {
        motion::progress(self.arrival.as_ref(), now)
    }

    fn animating(&self, now: Instant) -> bool {
        self.arrival.is_some_and(|fade| fade.running(now))
            || self
                .position
                .as_ref()
                .is_some_and(|glide| glide.running(now))
    }
}

struct LeavingRows {
    rows: Vec<(Address, f32, f32)>,
    highlight: f32,
    fade: Transition,
}

#[derive(Default)]
pub(crate) struct Menu {
    rows: Vec<Address>,
    animations: Vec<RowAnimation>,
    leaving: Option<LeavingRows>,
    status: Option<&'static str>,
    status_arrival: Option<Transition>,
    close: Option<Transition>,
    height: Option<Glide>,
    highlight: Option<Glide>,
    open: bool,
}

impl Menu {
    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn rows(&self) -> &[Address] {
        if self.open { &self.rows } else { &[] }
    }

    pub fn loading(&mut self, selected: usize, motion: Motion, now: Instant) {
        self.update(
            Vec::new(),
            Some("Finding addresses…"),
            selected,
            motion,
            now,
        );
    }

    pub fn show(
        &mut self,
        rows: Vec<Address>,
        selected: usize,
        motion: Motion,
        now: Instant,
    ) -> usize {
        let status = rows.is_empty().then_some("No matching addresses");
        self.update(rows, status, selected, motion, now)
    }

    fn update(
        &mut self,
        mut rows: Vec<Address>,
        status: Option<&'static str>,
        selected: usize,
        motion: Motion,
        now: Instant,
    ) -> usize {
        let old_height = self.height_at(now);
        let previous = self.rows();
        let was_empty = previous.is_empty();
        let mut next = 0;
        if let Some(person) = previous.get(selected)
            && let Some(index) = rows.iter().position(|row| same_address(row, person))
        {
            next = selected.min(rows.len() - 1);
            let person = rows.remove(index);
            rows.insert(next, person);
        }
        if self.open && self.rows == rows && self.status == status {
            return next;
        }
        let leaving = if rows.is_empty() && !was_empty {
            motion.start(motion::QUICK, now).map(|fade| LeavingRows {
                rows: previous
                    .iter()
                    .zip(&self.animations)
                    .enumerate()
                    .map(|(index, (row, animation))| {
                        (
                            row.clone(),
                            animation.top(index, now),
                            animation.opacity(now),
                        )
                    })
                    .collect(),
                highlight: self.highlight_at(selected, now),
                fade,
            })
        } else {
            None
        };
        self.animations = rows
            .iter()
            .enumerate()
            .map(|(index, person)| {
                match previous.iter().position(|row| same_address(row, person)) {
                    Some(old) => {
                        let animation = &self.animations[old];
                        RowAnimation {
                            arrival: animation.arrival,
                            position: Glide::start(
                                &motion,
                                animation.top(old, now),
                                index as f32 * ROW,
                                0.,
                                motion::QUICK,
                                now,
                            ),
                        }
                    }
                    None => RowAnimation {
                        arrival: motion.start(motion::FADE, now),
                        ..RowAnimation::default()
                    },
                }
            })
            .collect();
        let resize_at = if leaving.is_some() {
            now + motion::QUICK
        } else {
            now
        };
        if leaving.is_some() || !rows.is_empty() || !self.open {
            self.leaving = leaving;
        }
        if status != self.status || !self.open {
            self.status_arrival = status.and_then(|_| motion.start(motion::FADE, now));
        }
        let height = panel_height(rows.len());
        if !self.open || height != panel_height(self.rows.len()) {
            self.height = if self.open || self.close.is_some() {
                Glide::start(&motion, old_height, height, 0., motion::FADE, resize_at)
            } else {
                None
            };
        }
        // Keep a highlight that was already travelling to the same slot.
        if next != selected || was_empty {
            self.highlight = None;
        }
        self.rows = rows;
        self.status = status;
        self.open = true;
        self.close = None;
        next
    }

    pub fn dismiss(&mut self, motion: Motion, now: Instant) -> bool {
        if !self.open {
            return false;
        }
        self.height = Glide::start(&motion, self.height_at(now), 0., 0., motion::FADE, now);
        self.close = motion.start(motion::FADE, now);
        self.open = false;
        true
    }

    pub fn move_selection(&mut self, from: usize, to: usize, motion: Motion, now: Instant) {
        let y = self.highlight_at(from, now);
        self.highlight = Glide::start(&motion, y, to as f32 * ROW, 0., motion::QUICK, now);
    }

    fn highlight_at(&self, selected: usize, now: Instant) -> f32 {
        self.highlight
            .as_ref()
            .map_or(selected as f32 * ROW, |glide| glide.value(now))
    }

    fn height_at(&self, now: Instant) -> f32 {
        self.height
            .as_ref()
            .map_or(panel_height(self.rows.len()), |glide| glide.value(now))
    }

    pub fn animating(&self, now: Instant) -> bool {
        self.close.is_some_and(|fade| fade.running(now))
            || self.animations.iter().any(|row| row.animating(now))
            || self.status_arrival.is_some_and(|fade| fade.running(now))
            || self
                .leaving
                .as_ref()
                .is_some_and(|rows| rows.fade.running(now))
            || self.height.as_ref().is_some_and(|glide| glide.running(now))
            || self
                .highlight
                .as_ref()
                .is_some_and(|glide| glide.running(now))
    }

    pub fn popup(
        &self,
        window: &mut Window,
        selected: usize,
        interactive: bool,
        theme: &Theme,
        on_row: impl Fn(usize, Div) -> gpui::AnyElement,
    ) -> Option<Div> {
        let now = Instant::now();
        if !self.open && !self.close.is_some_and(|fade| fade.running(now)) {
            return None;
        }
        if self.animating(now) {
            window.request_animation_frame();
        }
        let alpha = self.close.map_or(1., |fade| fade.remaining(now));
        let highlight = self.highlight_at(selected, now);
        Some(
            div()
                .w_full()
                .h(px(self.height_at(now)))
                .overflow_hidden()
                .py_1()
                .rounded_md()
                .border_1()
                .border_color(rgb(theme.border))
                .bg(rgb(theme.recipient_surface))
                .text_size(px(14.))
                .shadow_md()
                // Only the contents fade. The panel stays opaque even while
                // it covers text in the fields below the active line.
                .child(
                    div()
                        .relative()
                        .h_full()
                        .opacity(alpha)
                        .children(self.leaving.as_ref().and_then(|leaving| {
                            leaving.fade.running(now).then(|| {
                                div()
                                    .absolute()
                                    .size_full()
                                    .opacity(leaving.fade.remaining(now))
                                    .child(selection_band(leaving.highlight, theme))
                                    .children(leaving.rows.iter().map(|(person, top, opacity)| {
                                        recipient_row(person, *top, leaving.highlight, theme)
                                            .opacity(*opacity)
                                    }))
                            })
                        }))
                        .children(self.status.map(|status| {
                            row_base(0.)
                                .opacity(motion::progress(self.status_arrival.as_ref(), now))
                                .bg(rgb(theme.recipient_surface))
                                .text_color(rgb(theme.recipient_text.1))
                                .child(status)
                        }))
                        .children((!self.rows.is_empty()).then(|| selection_band(highlight, theme)))
                        .children(self.rows.iter().enumerate().map(|(index, person)| {
                            let animation = &self.animations[index];
                            let row =
                                recipient_row(person, animation.top(index, now), highlight, theme)
                                    .opacity(animation.opacity(now));
                            if self.open && interactive {
                                on_row(index, row)
                            } else {
                                row.into_any_element()
                            }
                        })),
                ),
        )
    }
}

fn panel_height(rows: usize) -> f32 {
    FRAME + ROW * rows.max(1) as f32
}

fn same_address(a: &Address, b: &Address) -> bool {
    a.email.eq_ignore_ascii_case(&b.email)
}

fn selection_band(top: f32, theme: &Theme) -> Div {
    div()
        .absolute()
        .top(px(top))
        .left(px(2.))
        .right(px(2.))
        .h(px(ROW))
        .rounded_sm()
        .bg(rgb(theme.selection))
}

fn recipient_row(person: &Address, top: f32, highlight: f32, theme: &Theme) -> Div {
    let selected = (1. - (top - highlight).abs() / ROW).max(0.);
    let (foreground, dimmed) = theme.recipient_text;
    let text = motion::mix(foreground, theme.on_selection, selected);
    let dim = motion::mix(dimmed, theme.on_selection_dim, selected);
    row_base(top)
        .text_color(rgb(text))
        .children(person.name.as_deref().map(|name| {
            div()
                .max_w(relative(0.48))
                .min_w_0()
                .truncate()
                .font_weight(FontWeight::MEDIUM)
                .child(display_safe(name))
        }))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .when(person.name.is_some(), |email| email.text_color(rgb(dim)))
                .child(display_safe(&person.email)),
        )
}

fn row_base(top: f32) -> Div {
    div()
        .absolute()
        .top(px(top))
        .left_0()
        .right_0()
        .h(px(ROW))
        .pl(px(TEXT_INSET - 1.))
        .pr(px(16.))
        .gap_2()
        .flex()
        .items_center()
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor::block_on;
    use futures::future::ready;

    #[test]
    fn a_ready_lookup_skips_loading_and_a_slow_one_shows_it() {
        assert_eq!(
            block_on(wait_for_lookup(ready(42), ready(()), || panic!())),
            42
        );
        let (send, reply) = futures::channel::oneshot::channel();
        assert_eq!(
            block_on(wait_for_lookup(reply, ready(()), || send.send(42).unwrap())).unwrap(),
            42
        );
    }

    #[test]
    fn an_empty_reply_fades_out_rows_while_its_status_arrives() {
        let start = Instant::now();
        let motion = Motion::new(true);
        let mut menu = Menu::default();
        menu.show(
            vec![Address {
                name: None,
                email: "ruben@example.test".into(),
            }],
            0,
            motion,
            start,
        );
        let changed = start + motion::FADE;
        menu.show(Vec::new(), 0, motion, changed);
        assert!(menu.rows().is_empty());
        let fade = menu.leaving.unwrap().fade;
        let arrival = menu.status_arrival.unwrap();
        assert_eq!(fade.remaining(changed), 1.);
        assert_eq!(arrival.progress(changed), 0.);
        assert_eq!(fade.remaining(changed + motion::QUICK), 0.);
        assert!(arrival.running(changed + motion::QUICK));
        assert_eq!(arrival.progress(changed + motion::FADE), 1.);
    }
}
