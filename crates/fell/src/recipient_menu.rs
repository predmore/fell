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
pub(crate) const EXCLUSION_LIMIT: usize = 200;
const ROW: f32 = 31.;
const FRAME: f32 = 10.; // top and bottom padding and border
const SWAP_DELAY: Duration = Duration::from_millis(60);
const PANEL_FADE: Duration = Duration::from_millis(40);

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
    arrival: Option<Arrival>,
    position: Option<Glide>,
}

impl RowAnimation {
    fn top(&self, index: usize, now: Instant) -> f32 {
        self.position
            .as_ref()
            .map_or(index as f32 * ROW, |glide| glide.value(now))
    }

    fn opacity(&self, now: Instant) -> f32 {
        Arrival::opacity(self.arrival, now)
    }

    fn animating(&self, now: Instant) -> bool {
        self.arrival
            .is_some_and(|arrival| arrival.fade.running(now))
            || self
                .position
                .as_ref()
                .is_some_and(|glide| glide.running(now))
    }
}

#[derive(Clone, Copy)]
struct Arrival {
    from: f32,
    fade: Transition,
}

impl Arrival {
    fn start(motion: Motion, from: f32, at: Instant) -> Option<Self> {
        motion
            .start(motion::FADE, at)
            .map(|fade| Self { from, fade })
    }

    fn opacity(arrival: Option<Self>, now: Instant) -> f32 {
        arrival.map_or(1., |arrival| {
            motion::lerp(arrival.from, 1., arrival.fade.progress(now))
        })
    }
}

struct Departure<T> {
    value: T,
    from: f32,
    fade: Transition,
}

impl<T> Departure<T> {
    fn opacity(&self, now: Instant) -> f32 {
        self.from * self.fade.remaining(now)
    }
}

struct LeavingRow {
    person: Address,
    top: f32,
    highlight: f32,
}

#[derive(Default)]
pub(crate) struct Menu {
    rows: Vec<Address>,
    animations: Vec<RowAnimation>,
    leaving_rows: Vec<Departure<LeavingRow>>,
    leaving_band: Option<Departure<f32>>,
    leaving_status: Option<Departure<&'static str>>,
    status: Option<&'static str>,
    status_arrival: Option<Arrival>,
    close: Option<Transition>,
    panel_fade: Option<Transition>,
    anchored_exit_height: Option<f32>,
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
        let previous = if self.open { &self.rows[..] } else { &[] };
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
        let highlight = self.highlight_at(selected, now);
        self.leaving_rows
            .retain(|row| self.open && row.fade.running(now));
        self.leaving_band = self
            .leaving_band
            .take()
            .filter(|band| self.open && band.fade.running(now) && rows.is_empty());
        self.leaving_status = self
            .leaving_status
            .take()
            .filter(|status| self.open && status.fade.running(now));
        let status_changed = !self.open || self.status != status;
        if let Some(fade) = motion.start(motion::QUICK, now) {
            for (index, (person, animation)) in previous.iter().zip(&self.animations).enumerate() {
                let from = animation.opacity(now);
                if from > 0. && !rows.iter().any(|row| same_address(row, person)) {
                    self.leaving_rows.push(Departure {
                        value: LeavingRow {
                            person: person.clone(),
                            top: animation.top(index, now),
                            highlight,
                        },
                        from,
                        fade,
                    });
                }
            }
            if rows.is_empty()
                && let Some(animation) = self.animations.get(selected).filter(|_| !was_empty)
            {
                self.leaving_band = Some(Departure {
                    value: highlight,
                    from: animation.opacity(now),
                    fade,
                });
            }
            if self.open
                && status_changed
                && let Some(text) = self.status
            {
                let from = Arrival::opacity(self.status_arrival, now);
                if from > 0. {
                    self.leaving_status = Some(Departure {
                        value: text,
                        from,
                        fade,
                    });
                }
            }
        }
        let row_arrival = if self
            .leaving_status
            .as_ref()
            .is_some_and(|status| status.opacity(now) > 0.2)
        {
            now + SWAP_DELAY
        } else {
            now
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
                    None => {
                        let leaving = self
                            .leaving_rows
                            .iter()
                            .find(|row| same_address(&row.value.person, person));
                        RowAnimation {
                            arrival: Arrival::start(
                                motion,
                                leaving.map_or(0., |row| row.opacity(now)),
                                row_arrival,
                            ),
                            position: leaving.and_then(|row| {
                                Glide::start(
                                    &motion,
                                    row.value.top,
                                    index as f32 * ROW,
                                    0.,
                                    motion::QUICK,
                                    now,
                                )
                            }),
                        }
                    }
                }
            })
            .collect();
        self.leaving_rows.retain(|leaving| {
            !rows
                .iter()
                .any(|row| same_address(&leaving.value.person, row))
        });
        let height = panel_height(rows.len());
        let resize_at = if height < old_height && !self.leaving_rows.is_empty() {
            now + motion::QUICK
        } else {
            now
        };
        if status_changed {
            let returning = self
                .leaving_status
                .take_if(|leaving| Some(leaving.value) == status);
            let arrival_at = if !self.leaving_rows.is_empty() {
                now + SWAP_DELAY
            } else {
                row_arrival
            };
            self.status_arrival = status.and_then(|_| {
                Arrival::start(
                    motion,
                    returning.map_or(0., |status| status.opacity(now)),
                    arrival_at,
                )
            });
        }
        if !self.open || height != panel_height(self.rows.len()) {
            self.height = if self.open || self.close.is_some() {
                Glide::start(&motion, old_height, height, 0., motion::FADE, resize_at)
            } else {
                None
            };
        }
        // Keep a highlight that was already travelling to the same slot.
        if was_empty || rows.is_empty() {
            self.highlight = None;
        } else if next != selected {
            self.highlight = Glide::start(
                &motion,
                highlight,
                next as f32 * ROW,
                0.,
                motion::QUICK,
                now,
            );
        }
        self.rows = rows;
        self.status = status;
        self.open = true;
        self.close = None;
        self.panel_fade = None;
        self.anchored_exit_height = None;
        next
    }

    pub fn dismiss(&mut self, motion: Motion, now: Instant) -> bool {
        if !self.open {
            return false;
        }
        self.height = Glide::start(&motion, self.height_at(now), 0., 0., motion::FADE, now);
        self.close = motion.start(motion::FADE, now);
        self.panel_fade = motion.start(PANEL_FADE, now + motion::FADE - PANEL_FADE);
        self.open = false;
        true
    }

    /// Keep the panel's shape as its compose row moves underneath it.
    pub fn dismiss_anchored(&mut self, motion: Motion, now: Instant) -> bool {
        if !self.open {
            return false;
        }
        self.anchored_exit_height = Some(self.height_at(now));
        self.height = None;
        self.close = motion.start(motion::QUICK, now);
        self.panel_fade = self.close;
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
        self.anchored_exit_height.unwrap_or_else(|| {
            self.height
                .as_ref()
                .map_or(panel_height(self.rows.len()), |glide| glide.value(now))
        })
    }

    pub fn animating(&self, now: Instant) -> bool {
        self.close.is_some_and(|fade| fade.running(now))
            || self.animations.iter().any(|row| row.animating(now))
            || self
                .status_arrival
                .is_some_and(|arrival| arrival.fade.running(now))
            || self.leaving_rows.iter().any(|row| row.fade.running(now))
            || self
                .leaving_status
                .as_ref()
                .is_some_and(|status| status.fade.running(now))
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
        let alpha = if self.anchored_exit_height.is_some() {
            1.
        } else {
            self.close.map_or(1., |fade| fade.remaining(now))
        };
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
                .opacity(self.panel_fade.map_or(1., |fade| fade.remaining(now)))
                .child(
                    div()
                        .relative()
                        .h_full()
                        .opacity(alpha)
                        .children(
                            self.leaving_band
                                .iter()
                                .filter(|band| band.fade.running(now))
                                .map(|band| {
                                    selection_band(band.value, theme).opacity(band.opacity(now))
                                }),
                        )
                        .children(
                            self.animations.get(selected).map(|row| {
                                selection_band(highlight, theme).opacity(row.opacity(now))
                            }),
                        )
                        .children(
                            self.leaving_rows
                                .iter()
                                .filter(|row| row.fade.running(now))
                                .map(|row| {
                                    let leaving = &row.value;
                                    recipient_row(
                                        &leaving.person,
                                        leaving.top,
                                        leaving.highlight,
                                        theme,
                                    )
                                    .opacity(row.opacity(now))
                                }),
                        )
                        .children(
                            self.leaving_status
                                .iter()
                                .filter(|status| status.fade.running(now))
                                .map(|status| status_row(status.value, status.opacity(now), theme)),
                        )
                        .children(self.status.map(|status| {
                            status_row(status, Arrival::opacity(self.status_arrival, now), theme)
                        }))
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

fn status_row(text: &'static str, opacity: f32, theme: &Theme) -> Div {
    row_base(0.)
        .opacity(opacity)
        .text_color(rgb(theme.recipient_text.1))
        .child(text)
}

fn selection_band(top: f32, theme: &Theme) -> Div {
    div()
        .absolute()
        .top(px(top))
        .left(px(2.))
        .right(px(2.))
        .h(px(ROW))
        .rounded_sm()
        .bg(rgb(theme.recipient_selection))
}

fn recipient_row(person: &Address, top: f32, highlight: f32, theme: &Theme) -> Div {
    let selected = (1. - (top - highlight).abs() / ROW).max(0.);
    let (foreground, dimmed) = theme.recipient_text;
    let (selected_foreground, selected_dimmed) = theme.recipient_selected_text;
    let text = motion::mix(foreground, selected_foreground, selected);
    let dim = motion::mix(dimmed, selected_dimmed, selected);
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
    fn rows_and_status_take_turns_in_the_same_slot() {
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
        let leaving = &menu.leaving_rows[0];
        let arrival = menu.status_arrival.unwrap();
        assert_eq!(leaving.opacity(changed), 1.);
        assert_eq!(Arrival::opacity(Some(arrival), changed + SWAP_DELAY), 0.);
        assert!(leaving.opacity(changed + SWAP_DELAY) < 0.2);
        assert_eq!(leaving.opacity(changed + motion::QUICK), 0.);
        let settled = changed + SWAP_DELAY + motion::FADE;
        assert_eq!(Arrival::opacity(Some(arrival), settled), 1.);

        menu.show(vec![person("ruben")], 0, motion, settled);
        let leaving = menu.leaving_status.as_ref().unwrap();
        assert_eq!(leaving.opacity(settled), 1.);
        assert_eq!(menu.animations[0].opacity(settled + SWAP_DELAY), 0.);
        assert!(leaving.opacity(settled + SWAP_DELAY) < 0.2);
        assert_eq!(leaving.opacity(settled + motion::QUICK), 0.);
        assert_eq!(
            menu.animations[0].opacity(settled + SWAP_DELAY + motion::FADE),
            1.
        );
    }

    #[test]
    fn a_departing_row_stays_in_place_while_the_survivor_moves() {
        let start = Instant::now();
        let motion = Motion::new(true);
        let mut menu = Menu::default();
        menu.show(vec![person("riya"), person("ruben")], 0, motion, start);
        let changed = start + motion::FADE / 2;
        let visible = menu.animations[0].opacity(changed);
        assert!((0. ..1.).contains(&visible));
        menu.show(vec![person("ruben")], 0, motion, changed);
        assert_eq!(menu.rows(), &[person("ruben")]);
        let halfway = changed + motion::QUICK / 2;
        let leaving = &menu.leaving_rows[0];
        assert_eq!(leaving.value.person, person("riya"));
        assert_eq!(leaving.value.top, 0.);
        assert_eq!(leaving.opacity(changed), visible);
        assert_eq!(leaving.opacity(halfway), visible * 0.25);
        assert_eq!(menu.animations[0].top(0, changed), ROW);
        assert!((0. ..ROW).contains(&menu.animations[0].top(0, halfway)));
        assert_eq!(menu.animations[0].top(0, changed + motion::QUICK), 0.);
        assert_eq!(menu.height_at(halfway), panel_height(2));
    }

    #[test]
    fn the_panel_becomes_transparent_before_its_last_pixels_collapse() {
        let start = Instant::now();
        let motion = Motion::new(true);
        let mut menu = Menu::default();
        menu.show(vec![person("ruben")], 0, motion, start);
        let dismissed = start + motion::FADE;
        assert!(menu.dismiss(motion, dismissed));
        assert!(menu.rows().is_empty());
        let fade = menu.panel_fade.unwrap();
        assert_eq!(fade.remaining(dismissed + motion::FADE - PANEL_FADE), 1.);
        let ending = dismissed + motion::FADE - PANEL_FADE / 2;
        assert!(menu.height_at(ending) < 2.);
        assert_eq!(fade.remaining(ending), 0.25);
        let end = dismissed + motion::FADE;
        assert_eq!(menu.height_at(end), 0.);
        assert_eq!(fade.remaining(end), 0.);
        assert!(!menu.animating(end));
    }

    #[test]
    fn compose_dismisses_at_the_current_height_and_fades_over_eighty_ms() {
        let start = Instant::now();
        let motion = Motion::new(true);
        let mut menu = Menu::default();
        menu.show(vec![person("ruben"), person("riya")], 0, motion, start);
        let dismissed = start + motion::FADE;
        let height = menu.height_at(dismissed);
        assert!(menu.dismiss_anchored(motion, dismissed));
        assert!(menu.rows().is_empty());
        let halfway = dismissed + motion::QUICK / 2;
        assert_eq!(menu.height_at(halfway), height);
        assert_eq!(menu.panel_fade.unwrap().remaining(halfway), 0.25);
        assert!(!menu.animating(dismissed + motion::QUICK));
    }

    fn person(name: &str) -> Address {
        Address {
            name: None,
            email: format!("{name}@example.test"),
        }
    }
}
