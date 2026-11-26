//! The key reference scrolls to whole rows, even after wheel input.
use std::time::Instant;

use gpui::prelude::*;
use gpui::{Context, Div, FontWeight, MouseButton, Pixels, Size, Window, div, px, rgb};

use super::{CARD_TOP_PX, FontOpt, Inbox, conversation_line_style};
use crate::keymap::{ACTION_SECTIONS, Action, KeyId, binding_text};
use crate::model::{Overlay, Session};
use crate::motion::{self, Glide, Motion};
use crate::theme::Theme;

const MAX_WIDTH: f32 = 520.;
const MAX_HEIGHT: f32 = 460.;
const INSET: f32 = 8.;
const LEFT: f32 = 14.;
const RIGHT: f32 = 24.;
const DETAIL_GAP: f32 = 8.;

#[derive(Default)]
pub(super) struct HelpScroll {
    page: usize,
    last: usize,
    row: f32,
    target: usize,
    drawn: f32,
    glide: Option<Glide>,
    wheel_remainder: f32,
}

impl HelpScroll {
    /// An open and a scroll key can arrive before the first paint.
    pub(super) fn prepare(&mut self, now: Instant, measure: impl FnOnce() -> Layout) {
        if self.page == 0 {
            let layout = measure();
            self.advance(layout.page, layout.total, layout.row, now);
        }
    }

    pub(super) fn on_key(&mut self, key: KeyId, motion: &Motion, now: Instant) -> bool {
        let half_page = (self.page / 2).max(1) as isize;
        let rows = match key {
            KeyId::Char('j') | KeyId::Down => 1,
            KeyId::Char('k') | KeyId::Up => -1,
            KeyId::Ctrl('d') => half_page,
            KeyId::Ctrl('u') => -half_page,
            _ => return false,
        };
        self.wheel_remainder = 0.;
        self.scroll(rows, motion, now);
        true
    }

    fn wheel(&mut self, rows: f32, motion: &Motion, now: Instant) {
        self.wheel_remainder += rows;
        let whole = self.wheel_remainder.trunc();
        self.wheel_remainder -= whole;
        let whole = whole as isize;
        let before = self.target;
        self.scroll(whole, motion, now);
        let applied = self.target as isize - before as isize;
        if applied != whole {
            self.wheel_remainder = 0.;
        }
    }

    fn scroll(&mut self, rows: isize, motion: &Motion, now: Instant) {
        let target = self.target.saturating_add_signed(rows).min(self.last);
        if target == self.target {
            return;
        }
        self.target = target;
        // Repeated input uses the latest destination and starts from what was
        // actually drawn, including when several keys land between frames.
        self.glide = Glide::start(
            motion,
            self.drawn,
            target as f32 * self.row,
            0.,
            motion::SLIDE,
            now,
        )
        .map(|glide| glide.gentle_beyond(3. * self.row));
    }

    pub(super) fn running(&self, now: Instant) -> bool {
        self.glide.is_some_and(|glide| glide.running(now))
    }

    fn advance(&mut self, page: usize, total: usize, row: f32, now: Instant) {
        self.page = page;
        self.last = total.saturating_sub(page);
        if self.target > self.last || self.row != row {
            self.target = self.target.min(self.last);
            self.glide = None;
        }
        self.row = row;
        if !self.running(now) {
            self.glide = None;
        }
        self.drawn = self
            .glide
            .map_or(self.target as f32 * row, |glide| glide.value(now))
            .min(self.last as f32 * row);
    }

    pub(super) fn card(
        &mut self,
        theme: &Theme,
        session: &Session,
        window: &Window,
        now: Instant,
        live: bool,
        cx: &mut Context<Inbox>,
    ) -> Option<(f32, Div)> {
        if session.overlay != Overlay::Help {
            return None;
        }
        let layout = Layout::for_window(theme, session, window);
        if live {
            self.advance(layout.page, layout.total, layout.row, now);
        }
        let moving = self.glide.is_some();
        let drawn = if moving {
            motion::snap(self.drawn, window.scale_factor())
        } else {
            self.drawn
        };
        let offset = drawn.min((layout.total - layout.page) as f32 * layout.row);
        Some((
            layout.top,
            card(theme, session, layout, offset, moving, live, cx),
        ))
    }
}

const SUBJECT_JOIN_HELP: &str = "Joined by subject · Fell matched the subject and participants because reply headers were missing. Space c s splits it off.";

pub(super) struct Layout {
    top: f32,
    width: f32,
    text_width: f32,
    header: f32,
    row: f32,
    line: f32,
    stacked: bool,
    details: Vec<(String, bool, usize)>,
    detail_rows: usize,
    detail_gap: f32,
    rows: Vec<HelpRow>,
    page: usize,
    total: usize,
}

impl Layout {
    pub(super) fn for_window(theme: &Theme, session: &Session, window: &Window) -> Self {
        let error = session.detail_text().map(|text| (text, true));
        let subject_join = session
            .messages
            .iter()
            .any(|message| message.joined_by_subject && !message.merged_by_you)
            && super::present::visible_panes(
                f32::from(window.viewport_size().width),
                session.pane,
                session.full_width,
            )
            .1;
        Self::measure(
            theme.ui_px,
            window.viewport_size(),
            error
                .into_iter()
                .chain(subject_join.then(|| (SUBJECT_JOIN_HELP.into(), false))),
            |error, width| {
                let style = conversation_line_style(window, theme, px(theme.ui_px));
                window
                    .text_system()
                    .shape_text(
                        error.to_owned().into(),
                        px(theme.ui_px),
                        &[style.to_run(error.len())],
                        Some(px(width)),
                        None,
                    )
                    .map_or(1, |lines| {
                        lines
                            .iter()
                            .map(|line| line.wrap_boundaries.len() + 1)
                            .sum()
                    })
            },
        )
    }

    fn measure(
        ui_px: f32,
        viewport: Size<Pixels>,
        details: impl IntoIterator<Item = (String, bool)>,
        count_detail_rows: impl Fn(&str, f32) -> usize,
    ) -> Self {
        let viewport_height = f32::from(viewport.height);
        let top = CARD_TOP_PX.min((viewport_height - 150.).max(0.));
        let width = (f32::from(viewport.width) - 24.).clamp(1., MAX_WIDTH);
        let text_width = (width - 2. - LEFT - RIGHT).max(1.);
        let line = (ui_px + 6.).ceil();
        let stacked = text_width < 21. * ui_px;
        let row = if stacked { 2. * line } else { line + 4. };
        let header = (ui_px + 26.).ceil();
        let height = (viewport_height - top - 16.).min(MAX_HEIGHT);
        let details: Vec<_> = details
            .into_iter()
            .map(|(text, error)| {
                let lines = count_detail_rows(&text, text_width);
                (text, error, lines)
            })
            .collect();
        let detail_rows: usize = details.iter().map(|(_, _, lines)| *lines).sum();
        let detail_gap = if detail_rows > 0 { DETAIL_GAP } else { 0. };
        let rows = help_rows().collect::<Vec<_>>();
        let total = rows.len() + detail_rows;
        let page =
            (((height - header - 2. - 2. * INSET - detail_gap) / row) as usize).clamp(1, total);
        Self {
            top,
            width,
            text_width,
            header,
            row,
            line,
            stacked,
            details,
            detail_rows,
            detail_gap,
            rows,
            page,
            total,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HelpRow {
    Section(&'static str),
    Action(Action),
}

fn help_rows() -> impl Iterator<Item = HelpRow> {
    ACTION_SECTIONS.iter().flat_map(|&(section, actions)| {
        std::iter::once(HelpRow::Section(section)).chain(
            actions
                .iter()
                .copied()
                .filter(|action| !matches!(action, Action::About | Action::Help))
                .map(HelpRow::Action),
        )
    })
}

fn thumb_height(body: f32, page: usize, total: usize, ui_px: f32) -> f32 {
    let minimum = ui_px * (16. / 15.);
    (body * page as f32 / total as f32).max(minimum).min(body)
}

fn card(
    theme: &Theme,
    session: &Session,
    layout: Layout,
    offset: f32,
    moving: bool,
    live: bool,
    cx: &mut Context<Inbox>,
) -> Div {
    let page_height = layout.page as f32 * layout.row;
    let body = page_height + layout.detail_gap;
    let last = (layout.total - layout.page) as f32 * layout.row;
    let first = (offset / layout.row).floor() as usize;
    let end = ((offset + page_height) / layout.row).ceil() as usize;
    // The detail gap can expose a sliver of the preceding row during a glide.
    let action_offset = if moving {
        offset - layout.detail_gap
    } else {
        offset
    };
    let action_start = ((action_offset / layout.row).floor() as usize).max(layout.detail_rows);
    div()
        .w(px(layout.width))
        .h(px(layout.header + body + 2. * INSET + 2.))
        .flex_none()
        .bg(rgb(theme.surface))
        .border_1()
        .border_color(rgb(theme.border))
        .rounded_md()
        .shadow_lg()
        .overflow_hidden()
        .when(live, |card| {
            let row = layout.row;
            card.on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_scroll_wheel(cx.listener(
                    move |inbox, event: &gpui::ScrollWheelEvent, _, cx| {
                        if inbox.session.overlay == Overlay::Help {
                            let rows = -f32::from(event.delta.pixel_delta(px(row)).y) / row;
                            inbox.help_scroll.wheel(
                                rows,
                                &inbox.motion.motion,
                                inbox.motion.clock.now(),
                            );
                            cx.stop_propagation();
                            cx.notify();
                        }
                    },
                ))
        })
        .child(
            div()
                .h(px(layout.header))
                .px(px(LEFT))
                .flex()
                .items_center()
                .border_b(px(1.))
                .border_color(rgb(theme.border))
                .font_weight(FontWeight::SEMIBOLD)
                .child("Keys"),
        )
        .child(
            div()
                .relative()
                .h(px(body + 2. * INSET))
                .child(
                    div()
                        .absolute()
                        .top(px(INSET))
                        .left(px(LEFT))
                        .right(px(RIGHT))
                        .h(px(body))
                        .overflow_hidden()
                        .children((first < layout.detail_rows).then(|| {
                            div()
                                .absolute()
                                .left_0()
                                .right_0()
                                .top(px(-offset))
                                .line_height(px(layout.row))
                                .children(layout.details.iter().map(|(text, error, lines)| {
                                    div()
                                        .h(px(layout.row * *lines as f32))
                                        .text_color(rgb(if *error {
                                            theme.error
                                        } else {
                                            theme.dimmed
                                        }))
                                        .child(text.clone())
                                }))
                        }))
                        .child(
                            div()
                                .absolute()
                                .left_0()
                                .right_0()
                                .top(px(
                                    action_start as f32 * layout.row + layout.detail_gap - offset
                                ))
                                .when(
                                    action_start == layout.detail_rows && layout.detail_rows > 0,
                                    |keys| {
                                        keys.child(
                                            div()
                                                .absolute()
                                                .left_0()
                                                .right_0()
                                                .top(px(-layout.detail_gap / 2.))
                                                .h(px(1.))
                                                .bg(rgb(theme.border)),
                                        )
                                    },
                                )
                                .children(
                                    layout
                                        .rows
                                        .iter()
                                        .copied()
                                        .skip(action_start - layout.detail_rows)
                                        .take(end.saturating_sub(action_start))
                                        .map(|row| help_row(theme, session, row, &layout)),
                                ),
                        )
                        .children((moving && offset > 0.).then(|| edge(theme, true, 14.)))
                        .children((moving && offset < last).then(|| edge(theme, false, 14.))),
                )
                .when(last > 0., |viewport| {
                    // This gutter is reserved at every scroll position.
                    let thumb = thumb_height(body, layout.page, layout.total, theme.ui_px);
                    viewport.child(
                        div()
                            .absolute()
                            .right(px(9.))
                            .top(px(INSET))
                            .w(px(4.))
                            .h(px(body))
                            .rounded_full()
                            .bg(rgb(theme.border))
                            .child(
                                div()
                                    .absolute()
                                    .top(px((body - thumb) * (offset / last)))
                                    .w_full()
                                    .h(px(thumb))
                                    .rounded_full()
                                    .bg(rgb(theme.dimmed)),
                            ),
                    )
                }),
        )
}

fn help_row(theme: &Theme, session: &Session, row: HelpRow, layout: &Layout) -> Div {
    let action = match row {
        HelpRow::Section(label) => {
            return div()
                .h(px(layout.row))
                .flex()
                .items_center()
                .line_height(px(layout.line))
                .text_color(rgb(theme.dimmed))
                .font_weight(FontWeight::SEMIBOLD)
                .child(label);
        }
        HelpRow::Action(action) => action,
    };
    div()
        .h(px(layout.row))
        .line_height(px(layout.line))
        .when(!layout.stacked, |row| {
            row.flex().items_center().gap(px(12.))
        })
        .child(
            div()
                .min_w_0()
                // Give GPUI a definite width on its first ellipsis measurement.
                .when(layout.stacked, |label| {
                    label.w(px(layout.text_width)).h(px(layout.line))
                })
                .when(!layout.stacked, |label| label.flex_1())
                .truncate()
                .child(action.label()),
        )
        .child(
            div()
                .when(layout.stacked, |keys| {
                    keys.w(px(layout.text_width)).h(px(layout.line))
                })
                .font_family_opt(theme.mono_family.clone())
                .text_color(rgb(theme.dimmed))
                .truncate()
                .child(binding_text(&session.keymap, action)),
        )
}

/// While a row crosses the viewport boundary, its text fades into the card.
pub(super) fn edge(theme: &Theme, top: bool, height: f32) -> Div {
    div()
        .absolute()
        .left_0()
        .right_0()
        .h(px(height))
        .when(top, |fade| fade.top_0())
        .when(!top, |fade| fade.bottom_0())
        .bg(crate::theme::scroll_fade(theme.surface, top))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::Keymap;
    use std::time::Duration;

    #[test]
    fn first_scroll_key_can_page_before_paint() {
        let now = Instant::now();
        let motion = Motion::new(false);
        let mut scroll = HelpScroll::default();
        assert_eq!(scroll.page, 0);
        let measure = || Layout::measure(13., gpui::size(px(520.), px(720.)), None, |_, _| 0);
        scroll.prepare(now, measure);
        let half_page = scroll.page / 2;
        assert!(half_page > 1);
        assert!(scroll.on_key(KeyId::Ctrl('d'), &motion, now));

        let layout = measure();
        scroll.advance(layout.page, layout.total, layout.row, now);
        assert_eq!(scroll.target, half_page);
        assert_eq!(scroll.drawn, half_page as f32 * layout.row);
    }

    #[test]
    fn section_headers_and_actions_follow_the_palette_order() {
        let keymap = Keymap::default();
        let rows: Vec<_> = help_rows().collect();
        let headings: Vec<_> = rows
            .iter()
            .filter_map(|row| match row {
                HelpRow::Section(label) => Some(*label),
                HelpRow::Action(_) => None,
            })
            .collect();
        assert_eq!(
            headings,
            [
                "Navigation",
                "Reading",
                "Compose",
                "Organize",
                "Attachments",
                "Search",
                "Agents",
                "Settings and help",
            ]
        );
        let actions: Vec<_> = rows
            .into_iter()
            .filter_map(|row| match row {
                HelpRow::Section(_) => None,
                HelpRow::Action(action) => Some(action),
            })
            .collect();
        let palette: Vec<_> = crate::keymap::palette_items(&keymap, "", &[])
            .into_iter()
            .map(|item| item.action)
            .filter(|action| !matches!(action, Action::About | Action::Help))
            .collect();
        assert_eq!(actions, palette);
    }

    #[test]
    fn scroll_can_settle_with_a_whole_header_on_either_edge() {
        let now = Instant::now();
        let motion = Motion::new(false);
        for ui_px in [12., 18.] {
            let layout = Layout::measure(ui_px, gpui::size(px(420.), px(380.)), None, |_, _| 0);
            let header = layout
                .rows
                .iter()
                .position(|row| *row == HelpRow::Section("Reading"))
                .unwrap();
            let mut scroll = HelpScroll::default();
            scroll.advance(layout.page, layout.total, layout.row, now);
            for first in [header + 1 - layout.page, header] {
                scroll.scroll((first - scroll.target) as isize, &motion, now);
                scroll.advance(layout.page, layout.total, layout.row, now);
                assert_eq!(scroll.drawn, first as f32 * layout.row);
                let header_top = header as f32 * layout.row - scroll.drawn;
                assert!(header_top >= 0.);
                assert!(header_top + layout.row <= layout.page as f32 * layout.row);
            }
        }
    }

    #[test]
    fn scrollbar_thumb_has_a_scaled_floor_bounded_by_the_track() {
        for ui_px in [12., 15., 18.] {
            let floor = thumb_height(50., 2, 100, ui_px);
            assert!((floor - 16. * ui_px / 15.).abs() < 0.001);
        }
        assert_eq!(thumb_height(300., 10, 50, 15.), 60.);
        assert_eq!(thumb_height(10., 1, 100, 15.), 10.);
    }

    #[test]
    fn keyboard_glides_retarget_from_the_drawn_offset() {
        let start = Instant::now();
        let motion = Motion::new(true);
        let mut scroll = HelpScroll::default();
        scroll.advance(16, 41, 25., start);
        assert!(scroll.on_key(KeyId::Char('j'), &motion, start));

        let next = start + Duration::from_millis(25);
        scroll.advance(16, 41, 25., next);
        let drawn = scroll.drawn;
        assert!(drawn > 0. && drawn < 25.);
        assert!(scroll.on_key(KeyId::Ctrl('d'), &motion, next));
        scroll.advance(16, 41, 25., next);
        assert_eq!(scroll.drawn, drawn);
        assert_eq!(scroll.target, 9);

        let end = next + motion::SLIDE;
        scroll.advance(16, 41, 25., end);
        assert_eq!(scroll.drawn, 225.);
        assert!(!scroll.running(end));
    }

    #[test]
    fn reduced_motion_and_wheel_stop_on_whole_rows() {
        let now = Instant::now();
        let motion = Motion::new(false);
        let mut scroll = HelpScroll::default();
        scroll.advance(16, 41, 25., now);
        scroll.wheel(0.4, &motion, now);
        assert_eq!(scroll.target, 0);
        scroll.wheel(0.7, &motion, now);
        assert_eq!(scroll.target, 1);
        for _ in 0..4 {
            scroll.on_key(KeyId::Ctrl('d'), &motion, now);
        }
        scroll.advance(16, 41, 25., now);
        assert_eq!(scroll.target + scroll.page, 41);
        assert_eq!(scroll.drawn, 625.);
        assert!(!scroll.running(now));

        scroll.on_key(KeyId::Ctrl('u'), &motion, now);
        scroll.on_key(KeyId::Up, &motion, now);
        scroll.advance(16, 41, 25., now);
        assert_eq!(scroll.drawn, 400.);
        assert!(!scroll.on_key(KeyId::Escape, &motion, now));
    }

    #[test]
    fn wheel_reverses_immediately_at_both_ends() {
        let now = Instant::now();
        let motion = Motion::new(false);
        let mut scroll = HelpScroll::default();
        scroll.advance(16, 41, 25., now);

        scroll.wheel(-2.9, &motion, now);
        assert_eq!(scroll.target, 0);
        scroll.wheel(1., &motion, now);
        assert_eq!(scroll.target, 1);

        scroll.scroll(scroll.last as isize, &motion, now);
        scroll.wheel(2.9, &motion, now);
        assert_eq!(scroll.target, scroll.last);
        scroll.wheel(-1., &motion, now);
        assert_eq!(scroll.target, scroll.last - 1);

        scroll.wheel(2.9, &motion, now);
        assert_eq!(scroll.target, scroll.last);
        scroll.wheel(-1., &motion, now);
        assert_eq!(scroll.target, scroll.last - 1);
    }
}
