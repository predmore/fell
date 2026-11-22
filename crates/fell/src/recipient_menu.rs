use std::time::{Duration, Instant};

use fell_core::Address;
use gpui::{Div, FontWeight, Styled, Window, div, prelude::*, px, relative, rgb};

use crate::motion::{self, Glide, Motion, Transition};
use crate::present::display_safe;
use crate::theme::Theme;

pub(crate) const QUERY_DELAY: Duration = Duration::from_millis(16);
pub(crate) const TEXT_INSET: f32 = 8.;
const ROW: f32 = 31.;
const FRAME: f32 = 10.; // top and bottom padding and border

#[derive(Default)]
pub(crate) struct Menu {
    rows: Vec<Address>,
    arrivals: Vec<Option<Transition>>,
    status: Option<&'static str>,
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

    pub fn loading(&mut self, motion: Motion, now: Instant) {
        if !self.open {
            self.update(Vec::new(), Some("Finding addresses…"), 0, motion, now);
        }
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
        self.arrivals = rows
            .iter()
            .map(
                |person| match previous.iter().position(|row| same_address(row, person)) {
                    Some(index) => self.arrivals.get(index).copied().flatten(),
                    None => motion.start(motion::QUICK, now),
                },
            )
            .collect();
        self.height = if self.open || self.close.is_some() {
            Glide::start(
                &motion,
                old_height,
                panel_height(rows.len()),
                0.,
                motion::FADE,
                now,
            )
        } else {
            None
        };
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
        let y = self
            .highlight
            .as_ref()
            .map_or(from as f32 * ROW, |glide| glide.value(now));
        self.highlight = Glide::start(&motion, y, to as f32 * ROW, 0., motion::QUICK, now);
    }

    fn height_at(&self, now: Instant) -> f32 {
        self.height
            .as_ref()
            .map_or(panel_height(self.rows.len()), |glide| glide.value(now))
    }

    pub fn animating(&self, now: Instant) -> bool {
        self.close.is_some_and(|fade| fade.running(now))
            || self.arrivals.iter().flatten().any(|fade| fade.running(now))
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
        let highlight = self
            .highlight
            .as_ref()
            .map_or(selected as f32 * ROW, |glide| glide.value(now));
        Some(
            div()
                .w_full()
                .h(px(self.height_at(now)))
                .overflow_hidden()
                .py_1()
                .rounded_md()
                .border_1()
                .border_color(rgb(theme.border))
                .bg(rgb(theme.surface))
                .text_size(px(14.))
                .shadow_md()
                // Only the contents fade. The panel stays opaque even while
                // it covers text in the fields below the active line.
                .child(
                    div()
                        .relative()
                        .opacity(alpha)
                        .children((!self.rows.is_empty()).then(|| {
                            div()
                                .absolute()
                                .top(px(highlight))
                                .left(px(2.))
                                .right(px(2.))
                                .h(px(ROW))
                                .rounded_sm()
                                .bg(rgb(theme.selection))
                                .child(
                                    div()
                                        .absolute()
                                        .right(px(6.))
                                        .top(px(8.))
                                        .w(px(2.))
                                        .h(px(15.))
                                        .bg(rgb(theme.on_selection_accent)),
                                )
                        }))
                        .children(
                            self.status.map(|status| {
                                row_base().text_color(rgb(theme.dimmed)).child(status)
                            }),
                        )
                        .children(self.rows.iter().enumerate().map(|(index, person)| {
                            let (text, dim) = if selected == index {
                                (theme.on_selection, theme.on_selection_dim)
                            } else {
                                (theme.foreground, theme.dimmed)
                            };
                            let row = row_base()
                                .opacity(motion::progress(self.arrivals[index].as_ref(), now))
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
                                        .when(person.name.is_some(), |email| {
                                            email.text_color(rgb(dim))
                                        })
                                        .child(display_safe(&person.email)),
                                );
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

fn row_base() -> Div {
    div()
        .h(px(ROW))
        .pl(px(TEXT_INSET - 1.))
        .pr(px(16.))
        .gap_2()
        .flex()
        .items_center()
}
