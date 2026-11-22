use std::time::Instant;

use fell_core::Address;
use gpui::{Div, FontWeight, Pixels, Styled, Window, div, prelude::*, px, relative, rgb};

use crate::motion::{self, Motion, Transition};
use crate::present::display_safe;

#[derive(Default)]
pub(crate) struct Menu {
    rows: Vec<Address>,
    transition: Option<Transition>,
    leaving: bool,
    initial: f32,
}

impl Menu {
    pub fn rows(&self) -> &[Address] {
        if self.leaving { &[] } else { &self.rows }
    }

    pub fn update(&mut self, rows: Vec<Address>, motion: Motion, now: Instant) {
        if self.rows() == rows {
            return;
        }
        self.leaving = rows.is_empty();
        self.initial = if self.rows.is_empty() {
            0.
        } else {
            motion::ARRIVE_FROM
        };
        self.transition = motion.start(motion::FADE, now);
        if !self.leaving || self.transition.is_none() {
            self.rows = rows;
        }
    }

    pub fn finish_opening(&mut self) {
        if !self.leaving {
            self.transition = None;
        }
    }

    pub fn animating(&self, now: Instant) -> bool {
        self.transition.is_some_and(|fade| fade.running(now))
    }

    pub fn visible(&mut self, window: &mut Window, now: Instant) -> (&[Address], f32, f32) {
        if self.transition.is_some_and(|fade| fade.running(now)) {
            window.request_animation_frame();
        } else {
            self.transition = None;
            if self.leaving {
                self.rows.clear();
            }
        }
        let alpha = if self.leaving {
            self.transition.map_or(0., |fade| fade.remaining(now))
        } else {
            motion::lerp(
                self.initial,
                1.,
                motion::progress(self.transition.as_ref(), now),
            )
        };
        let inset = if self.initial == 0. && !self.leaving {
            3. * (1. - alpha)
        } else {
            0.
        };
        (&self.rows, alpha, inset)
    }
}

pub(crate) struct Colors {
    pub surface: u32,
    pub border: u32,
    pub text: u32,
    pub dim: u32,
    pub selection: u32,
    pub accent: u32,
}

pub(crate) fn popup(
    people: &[Address],
    selected: Option<usize>,
    colors: Colors,
    alpha: f32,
    width: Pixels,
    on_row: impl Fn(usize, Div) -> gpui::AnyElement,
) -> Div {
    div()
        .w(width)
        .py_1()
        .rounded_md()
        .border_1()
        .border_color(rgb(colors.border))
        .bg(rgb(colors.surface))
        .text_color(rgb(colors.text))
        .text_size(px(14.))
        .shadow_md()
        .opacity(alpha)
        .children(people.iter().enumerate().map(|(index, person)| {
            let active = selected == Some(index);
            let row = div()
                .h(px(31.))
                .px_2()
                .mx_1()
                .gap_2()
                .flex()
                .items_center()
                .rounded_sm()
                .when(active, |row| row.bg(rgb(colors.selection)))
                .child(
                    div()
                        .flex_none()
                        .w(px(2.))
                        .h(px(15.))
                        .when(active, |mark| mark.bg(rgb(colors.accent))),
                )
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
                            email.text_color(rgb(colors.dim))
                        })
                        .child(display_safe(&person.email)),
                );
            on_row(index, row)
        }))
}
